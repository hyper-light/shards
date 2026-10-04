//! How a pull looks on a colour terminal: shards' own, in hyperlight's design
//! (shards_tui). The shards mark turning beside what is being pulled; a hairline bar a
//! layer, each fitted to the width there is; how fast it goes and what is left; the
//! microVM it becomes. A frame is drawn on each event from the daemon and every
//! [`TICK`] between, over the last, at the terminal's size then.
//!
//! Off a terminal, or without colour, none of this runs: the client prints `docker
//! pull`'s lines as they come.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use shards_ipc::Progress;
use shards_tui::bar::{self, Fill};
use shards_tui::canvas::Canvas;
use shards_tui::frame::Frame;
use shards_tui::layout::{self, Part};
use shards_tui::motion::{self, Clock};
use shards_tui::rate::Rate;
use shards_tui::text;
use shards_tui::tokens::{self, Paint};

/// Between frames: 12 a second, slow enough to cost nothing, fast enough to read as
/// motion (the site paints its studies at 30).
pub const TICK: Duration = Duration::from_millis(83);

/// What the display is told: the daemon's events, and text to print below the frame.
pub enum Shown {
    Progress(Progress),
    Out(Vec<u8>),
    Err(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    Arriving,
    Here,
    Verified,
}

struct Layer {
    digest: String,
    size: u64,
    got: u64,
    state: State,
}

/// A pull, as it stands, and how to draw it.
pub struct Pull {
    reference: String,
    layers: Vec<Layer>,
    building: bool,
    done: Option<(String, bool, bool)>,
    clock: Clock,
    rate: Rate,
    /// When the bytes stopped coming, for the summary.
    finished_at: Option<f64>,
    paint: Paint,
    ascii: bool,
    mark: Canvas,
    frame: Frame,
}

/// The terminal's rows and columns now.
pub type Size = (usize, usize);

impl Pull {
    pub fn new(truecolor: bool, east_asian: bool, reduced: bool) -> Pull {
        Pull {
            reference: String::new(),
            layers: Vec::new(),
            building: false,
            done: None,
            clock: Clock::new(reduced),
            rate: Rate::new(),
            finished_at: None,
            paint: Paint { truecolor },
            ascii: east_asian,
            mark: Canvas::new(8, 4),
            frame: Frame::new(),
        }
    }

    fn total(&self) -> (u64, u64) {
        self.layers
            .iter()
            .fold((0, 0), |(got, all), l| (got + l.got.min(l.size), all + l.size))
    }

    pub fn apply(&mut self, event: Progress) {
        let t = self.clock.t();
        let mut set = |d: &str, f: &dyn Fn(&mut Layer)| {
            if let Some(l) = self.layers.iter_mut().find(|l| l.digest == d) {
                f(l);
            }
        };
        match event {
            Progress::Pulling { reference, .. } => self.reference = reference,
            Progress::Layers(layers) => {
                self.layers = layers
                    .into_iter()
                    .map(|(digest, size)| Layer {
                        digest,
                        size,
                        got: 0,
                        state: State::Waiting,
                    })
                    .collect();
            }
            Progress::Have(d) => set(&d, &|l| {
                l.state = State::Here;
                l.got = l.size;
            }),
            Progress::Bytes(d, n) => set(&d, &|l| {
                l.state = State::Arriving;
                l.got = n;
            }),
            Progress::Verified(d) => set(&d, &|l| {
                l.state = State::Verified;
                l.got = l.size;
            }),
            Progress::Building => {
                self.building = true;
                self.finished_at.get_or_insert(t);
            }
            Progress::Done {
                digest,
                unchanged,
                bootable,
            } => {
                self.building = false;
                self.finished_at.get_or_insert(t);
                self.done = Some((digest, unchanged, bootable));
            }
        }
        // Only what came over the network counts toward the rate.
        let fetched: u64 = self
            .layers
            .iter()
            .filter(|l| l.state != State::Here)
            .map(|l| l.got.min(l.size))
            .sum();
        self.rate.at(t, fetched);
    }

    /// Draws the frame for now, at `size`, over the last.
    pub fn draw(&mut self, size: Size, out: &mut impl std::io::Write) {
        let (rows, cols) = (size.0.max(4), size.1.max(20));
        let t = self.clock.t();
        motion_mark(&mut self.mark, t);
        let lines = self.compose(rows, cols, t);
        self.frame.begin();
        for (line, width) in &lines {
            self.frame.put(line, *width);
        }
        // Only the lines that changed are written; nothing, if none did.
        self.frame.end(cols, out);
    }

    /// Whether anything on screen moves by itself: a layer arriving, the disk being
    /// made, or the mark turning. When nothing does, frames come only with events.
    pub fn moving(&self, size: Size) -> bool {
        if self.clock.reduced() || self.done.is_some() {
            return false;
        }
        let mark = size.1 >= 56 && size.0 >= 14;
        mark || self.building || self.layers.iter().any(|l| l.state == State::Arriving)
    }

    /// What follows is printed below the frame.
    pub fn leave(&mut self) {
        self.frame.leave();
    }

    /// The frame's lines, each with its visible width: never wider than `cols`, never
    /// more than `rows - 1`, so it redraws in place.
    fn compose(&self, rows: usize, cols: usize, t: f64) -> Vec<(String, usize)> {
        let p = &self.paint;
        let mut lines: Vec<(String, usize)> = Vec::new();
        let (got, all) = self.total();
        // The header: the mark beside it where there is room.
        let with_mark = cols >= 56 && rows >= 14;
        let indent = if with_mark { 12 } else { 2 };
        let room = cols.saturating_sub(indent + 1);
        let mut header: Vec<(String, usize)> = Vec::new();
        {
            let mut s = String::new();
            p.bold(&mut s, true);
            p.fg(&mut s, tokens::FOREGROUND);
            s.push_str("shards");
            p.bold(&mut s, false);
            let brow = text::eyebrow("pull");
            let mut w = 6;
            if room >= 6 + 4 + brow.chars().count() {
                p.fg(&mut s, tokens::SUBTLE);
                let _ = write!(s, "  ·  ");
                p.fg(&mut s, tokens::EYEBROW);
                s.push_str(&brow);
                w += 5 + brow.chars().count();
            }
            header.push((s, w));
        }
        {
            let name = layout::familiar(&self.reference);
            let name = if name.is_empty() { "…" } else { name };
            let shown = layout::clip(name, room);
            let mut s = String::new();
            p.bold(&mut s, true);
            p.fg(&mut s, tokens::BRIGHT);
            s.push_str(&shown);
            p.bold(&mut s, false);
            header.push((s, shown.chars().count()));
        }
        if !self.layers.is_empty() {
            let n = self.layers.len();
            let facts = [
                format!(
                    "{n} {} · {}",
                    if n == 1 { "layer" } else { "layers" },
                    text::bytes(all)
                ),
                format!("{n}L · {}", text::bytes_short(all)),
            ];
            let fact = facts
                .iter()
                .find(|f| f.chars().count() <= room)
                .cloned()
                .unwrap_or_default();
            let mut s = String::new();
            p.fg(&mut s, tokens::MUTED);
            s.push_str(&fact);
            header.push((s, fact.chars().count()));
        }
        // How it goes: rate, its recent history, what is left.
        if self.done.is_none() && all > 0 {
            let r = self.rate.bytes_per_second();
            let eta = self.rate.eta(all.saturating_sub(got));
            let mut parts = vec![
                Part::text(5, &[&format!("{:>3.0}%", got as f64 * 100.0 / all as f64)]),
                Part::text(4, &[&text::rate(r), ""]),
                Part::text(
                    2,
                    &[
                        &eta.map(|s| format!("{} left", text::duration(s)))
                            .unwrap_or_default(),
                        "",
                    ],
                ),
            ];
            parts.retain(|p| p.forms.first().is_some_and(|f| f.0 > 0));
            let spark = (room.saturating_sub(30)).min(12);
            let fit = layout::fit(&parts, 2, room, (if spark >= 4 { 4 } else { 0 }, spark));
            let mut s = String::new();
            let mut w = 0;
            for (part, &c) in parts.iter().zip(&fit.chosen) {
                if let Some((n, form)) = part.forms.get(c).filter(|f| f.0 > 0) {
                    if w > 0 {
                        s.push_str("  ");
                        w += 2;
                    }
                    p.fg(&mut s, if w == 0 { tokens::FOREGROUND } else { tokens::MUTED });
                    s.push_str(form);
                    w += n;
                }
            }
            if fit.elastic > 0 {
                s.push_str("  ");
                self.rate.sparkline(&mut s, p, fit.elastic);
                w += 2 + fit.elastic;
            }
            header.push((s, w));
        }
        let mark_rows = if with_mark { self.mark.rows() } else { 0 };
        for i in 0..header.len().max(mark_rows) {
            let mut s = String::from("  ");
            let mut w = 2;
            if with_mark {
                if i < mark_rows {
                    self.mark.row(i, p, &mut s);
                } else {
                    s.push_str(&" ".repeat(8));
                }
                s.push_str("  ");
                w += 10;
            }
            if let Some((h, hw)) = header.get(i) {
                s.push_str(h);
                w += hw;
            }
            lines.push((s, w));
        }
        lines.push((String::new(), 0));
        // The layers: as many as fit, the settled ones folded into a count past that.
        let budget = rows.saturating_sub(lines.len() + 5).max(1);
        let settled = self
            .layers
            .iter()
            .filter(|l| matches!(l.state, State::Here | State::Verified))
            .count();
        let fold = self.layers.len() > budget && settled > 0;
        if fold {
            let bytes: u64 = self
                .layers
                .iter()
                .filter(|l| matches!(l.state, State::Here | State::Verified))
                .map(|l| l.size)
                .sum();
            let words = [
                format!("{settled} layers settled  {}", text::bytes(bytes)),
                format!("{settled} settled"),
            ];
            let word = words
                .iter()
                .find(|w| w.chars().count() + 4 <= cols)
                .cloned()
                .unwrap_or_default();
            let mut s = String::from("  ");
            p.fg(&mut s, tokens::SAGE);
            s.push(if self.ascii { '*' } else { '●' });
            s.push(' ');
            p.fg(&mut s, tokens::MUTED);
            s.push_str(&word);
            lines.push((s, 4 + word.chars().count()));
        }
        let shown = self
            .layers
            .iter()
            .enumerate()
            .filter(|(_, l)| !fold || !matches!(l.state, State::Here | State::Verified))
            .take(budget.saturating_sub(usize::from(fold)));
        for (i, layer) in shown {
            lines.push(self.layer_line(i, layer, cols, t));
        }
        // The microVM it becomes.
        if self.building || self.done.as_ref().is_some_and(|d| d.2) {
            lines.push((String::new(), 0));
            let mut s = String::from("  ");
            let vm = if self.ascii { '#' } else { '◆' };
            let words = if self.building {
                "folding layers into one disk"
            } else {
                "ready to boot"
            };
            let fits = cols >= 14 + words.chars().count();
            if self.building {
                p.fg(
                    &mut s,
                    tokens::mix(tokens::LAVENDER, tokens::BRIGHT, motion::breath(t, 1.6, 0.0)),
                );
                s.push(vm);
                p.fg(&mut s, tokens::FOREGROUND);
                s.push_str(" microvm");
                if fits {
                    s.push_str("  ");
                    for (k, ch) in words.chars().enumerate() {
                        // A light running through the words.
                        let at = k as f64 / words.chars().count() as f64;
                        let lit = motion::glint(at, motion::travel(t, 0.5, 0.3, 0.0), 0.08);
                        p.fg(&mut s, tokens::mix(tokens::SUBTLE, tokens::LAVENDER, lit));
                        s.push(ch);
                    }
                }
            } else {
                p.fg(&mut s, tokens::SAGE);
                s.push(vm);
                p.fg(&mut s, tokens::FOREGROUND);
                s.push_str(" microvm");
                if fits {
                    p.fg(&mut s, tokens::MUTED);
                    s.push_str("  ");
                    s.push_str(words);
                }
            }
            lines.push((s, 2 + 9 + if fits { 2 + words.chars().count() } else { 0 }));
        }
        // The close: two beats, then what it took.
        if let Some((digest, unchanged, bootable)) = &self.done {
            lines.push((String::new(), 0));
            let n = self.layers.len();
            let layers = if n == 1 {
                "1 layer".to_string()
            } else {
                format!("{n} layers")
            };
            let said = match (unchanged, bootable) {
                (true, _) => [
                    String::from("Up to date. Nothing new to fetch."),
                    String::from("Up to date."),
                ],
                (false, true) => [format!("Ready. {layers}, one microVM."), String::from("Ready.")],
                (false, false) => [
                    format!("Stored. {layers}, for another platform."),
                    String::from("Stored."),
                ],
            };
            let words = said
                .iter()
                .find(|w| w.chars().count() + 2 <= cols)
                .cloned()
                .unwrap_or_default();
            let mut s = String::from("  ");
            p.bold(&mut s, true);
            p.fg(&mut s, tokens::BRIGHT);
            s.push_str(&words);
            p.bold(&mut s, false);
            lines.push((s, 2 + words.chars().count()));
            let took = self.finished_at.unwrap_or(t);
            let fetched: u64 = self
                .layers
                .iter()
                .filter(|l| l.state != State::Here)
                .map(|l| l.size)
                .sum();
            let mut facts = vec![Part::text(
                3,
                &[
                    &layout::clip(digest, 71),
                    &digest.chars().take(19).collect::<String>(),
                    "",
                ],
            )];
            if fetched > 0 && took > 0.0 {
                facts.insert(
                    0,
                    Part::text(
                        4,
                        &[
                            &format!(
                                "{} in {} · {}",
                                text::bytes(fetched),
                                text::duration(took),
                                text::rate(fetched as f64 / took)
                            ),
                            &format!("{} in {}", text::bytes(fetched), text::duration(took)),
                            "",
                        ],
                    ),
                );
            }
            let fit = layout::fit(&facts, 2, cols.saturating_sub(2), (0, 0));
            let mut s = String::from("  ");
            let mut w = 2;
            for (part, &c) in facts.iter().zip(&fit.chosen) {
                if let Some((n, form)) = part.forms.get(c).filter(|f| f.0 > 0) {
                    if w > 2 {
                        s.push_str("  ");
                        w += 2;
                    }
                    p.fg(&mut s, tokens::SUBTLE);
                    s.push_str(form);
                    w += n;
                }
            }
            lines.push((s, w));
        }
        // Never taller than the screen: a frame scrolled past its top cannot be redrawn.
        lines.truncate(rows.saturating_sub(1));
        lines
    }

    fn layer_line(&self, i: usize, layer: &Layer, cols: usize, t: f64) -> (String, usize) {
        let p = &self.paint;
        let (dot_color, dot) = match layer.state {
            State::Waiting => (tokens::SUBTLE, if self.ascii { '.' } else { '·' }),
            // Under way: amber, breathing.
            State::Arriving => (
                tokens::mix(
                    tokens::AMBER,
                    tokens::BRIGHT,
                    0.35 * motion::breath(t, 1.6, i as f64 * 0.7),
                ),
                if self.ascii { 'o' } else { '◌' },
            ),
            State::Here => (tokens::MUTED, if self.ascii { '*' } else { '●' }),
            State::Verified => (tokens::SAGE, if self.ascii { '*' } else { '●' }),
        };
        let hex = layer
            .digest
            .split_once(':')
            .map_or(layer.digest.as_str(), |(_, h)| h);
        let (long, short) = (hex.get(..12).unwrap_or(hex), hex.get(..6).unwrap_or(hex));
        let sizes = match layer.state {
            State::Arriving => [
                format!("{} / {}", text::bytes(layer.got), text::bytes(layer.size)),
                format!(
                    "{}/{}",
                    text::bytes_short(layer.got),
                    text::bytes_short(layer.size)
                ),
            ],
            _ => [text::bytes(layer.size), text::bytes_short(layer.size)],
        };
        let status: &[&str] = match layer.state {
            State::Waiting => &["waiting", ""],
            State::Arriving => &["", ""],
            State::Here => &["here already", "here", ""],
            State::Verified => &["verified", if self.ascii { "ok" } else { "✓" }, ""],
        };
        let parts = [
            Part::text(9, &["x"]),
            Part::text(3, &[long, short, ""]),
            Part::text(5, &[&sizes[0], &sizes[1], ""]),
            Part::text(2, status),
        ];
        let fit = layout::fit(&parts, 2, cols.saturating_sub(4), (8, 48));
        let mut s = String::from("    ");
        let mut w = 4;
        let pick = |k: usize| -> Option<&(usize, String)> {
            parts
                .get(k)
                .and_then(|part| part.forms.get(*fit.chosen.get(k)?))
                .filter(|f| f.0 > 0)
        };
        p.fg(&mut s, dot_color);
        s.push(dot);
        w += 1;
        if let Some((n, id)) = pick(1) {
            p.fg(&mut s, tokens::SUBTLE);
            s.push_str("  ");
            s.push_str(id);
            w += 2 + n;
        }
        if fit.elastic > 0 {
            s.push_str("  ");
            let fill = match layer.state {
                State::Waiting => Fill::Waiting,
                State::Arriving if layer.size == 0 => Fill::Unknown,
                State::Arriving => Fill::Going(layer.got as f64 / layer.size as f64),
                State::Here | State::Verified => Fill::Done,
            };
            bar::draw(&mut s, p, fit.elastic, fill, t, i as f64 * 0.37);
            w += 2 + fit.elastic;
        }
        if let Some((n, size)) = pick(2) {
            p.fg(&mut s, tokens::MUTED);
            s.push_str("  ");
            s.push_str(size);
            w += 2 + n;
        }
        if let Some((n, word)) = pick(3) {
            p.fg(
                &mut s,
                if layer.state == State::Verified {
                    tokens::SAGE
                } else {
                    tokens::SUBTLE
                },
            );
            s.push_str("  ");
            s.push_str(word);
            w += 2 + n;
        }
        (s, w)
    }
}

fn motion_mark(mark: &mut Canvas, t: f64) {
    shards_tui::mark::draw(mark, t);
}

/// Runs the display until the channel closes: a frame on each event and each tick, at
/// the terminal's size then; text from the daemon printed below the last frame.
pub fn run(events: std::sync::mpsc::Receiver<Shown>, mut pull: Pull, size: impl Fn() -> Size) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut stdout = std::io::stdout().lock();
    let mut started = false;
    loop {
        match events.recv_timeout(TICK) {
            Ok(Shown::Progress(event)) => {
                pull.apply(event);
                started = true;
                pull.draw(size(), &mut stdout);
            }
            Ok(Shown::Out(bytes)) => {
                if started {
                    pull.draw(size(), &mut stdout);
                    pull.leave();
                    started = false;
                }
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            Ok(Shown::Err(bytes)) => {
                if started {
                    pull.draw(size(), &mut stdout);
                    pull.leave();
                    started = false;
                }
                let _ = std::io::stderr().write_all(&bytes);
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = size();
                if started && pull.moving(now) {
                    pull.draw(now, &mut stdout);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if started {
                    pull.draw(size(), &mut stdout);
                    pull.leave();
                }
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn seen(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for ch in s.chars() {
            match (esc, ch) {
                (false, '\x1b') => esc = true,
                (true, c) if c.is_ascii_alphabetic() => esc = false,
                (true, _) => {}
                (false, ch) => out.push(ch),
            }
        }
        out
    }

    fn pulled(layers: usize) -> Pull {
        let mut p = Pull::new(true, false, true);
        p.apply(Progress::Pulling {
            reference: "docker.io/library/python:3.13".into(),
            repository: "library/python".into(),
        });
        p.apply(Progress::Layers(
            (0..layers)
                .map(|i| (format!("sha256:{i:064x}"), 1_000_000 * (i as u64 + 1)))
                .collect(),
        ));
        p
    }

    #[test]
    fn every_line_fits_every_terminal_and_says_what_it_can() {
        for cols in [20usize, 32, 48, 56, 80, 120, 240] {
            for rows in [6usize, 14, 40] {
                let mut p = pulled(6);
                p.apply(Progress::Have(format!("sha256:{:064x}", 0)));
                p.apply(Progress::Bytes(format!("sha256:{:064x}", 1), 700_000));
                p.apply(Progress::Verified(format!("sha256:{:064x}", 2)));
                for phase in 0..3 {
                    if phase == 1 {
                        p.apply(Progress::Building);
                    }
                    if phase == 2 {
                        p.apply(Progress::Done {
                            digest: format!("sha256:{:064x}", 7),
                            unchanged: false,
                            bootable: true,
                        });
                    }
                    let lines = p.compose(rows, cols, 1.0);
                    assert!(lines.len() < rows, "{cols}x{rows}: {} lines", lines.len());
                    for (line, width) in &lines {
                        let text = seen(line);
                        assert_eq!(text.chars().count(), *width, "{cols}x{rows}: {text:?}");
                        assert!(*width <= cols, "{cols}x{rows}: {text:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_wide_terminal_shows_the_whole_story() {
        let mut p = pulled(3);
        p.apply(Progress::Have(format!("sha256:{:064x}", 0)));
        p.apply(Progress::Bytes(format!("sha256:{:064x}", 1), 500_000));
        let text: String = p
            .compose(40, 120, 0.0)
            .iter()
            .map(|(l, _)| seen(l) + "\n")
            .collect();
        assert!(text.contains("shards  ·  P U L L"), "{text}");
        assert!(text.contains("python:3.13"), "{text}");
        assert!(text.contains("3 layers · 6.00 MB"), "{text}");
        assert!(text.contains("here already"), "{text}");
        assert!(text.contains("500 kB / 2.00 MB"), "{text}");
        p.apply(Progress::Verified(format!("sha256:{:064x}", 1)));
        p.apply(Progress::Verified(format!("sha256:{:064x}", 2)));
        p.apply(Progress::Building);
        let text: String = p
            .compose(40, 120, 0.0)
            .iter()
            .map(|(l, _)| seen(l) + "\n")
            .collect();
        assert!(text.contains("microvm  folding layers into one disk"), "{text}");
        p.apply(Progress::Done {
            digest: "sha256:abc".into(),
            unchanged: false,
            bootable: true,
        });
        let text: String = p
            .compose(40, 120, 0.0)
            .iter()
            .map(|(l, _)| seen(l) + "\n")
            .collect();
        assert!(text.contains("microvm  ready to boot"), "{text}");
        assert!(text.contains("Ready. 3 layers, one microVM."), "{text}");
        assert!(text.contains("sha256:abc"), "{text}");
    }

    #[test]
    fn many_layers_fold_what_has_settled() {
        let mut p = pulled(40);
        for i in 0..30 {
            p.apply(Progress::Verified(format!("sha256:{i:064x}")));
        }
        let lines = p.compose(20, 100, 0.0);
        assert!(lines.len() < 20);
        let text: String = lines.iter().map(|(l, _)| seen(l) + "\n").collect();
        assert!(text.contains("30 layers settled"), "{text}");
    }
}
