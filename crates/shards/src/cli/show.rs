//! How a pull looks on a colour terminal: shards' own, in hyperlight's design
//! (shards_tui). The brand at its head, the mark beside the word set in glass; the image
//! as a stack of glass plates, each a layer as large as its share, filling as its bytes
//! arrive and pouring into the disk the microVM boots from; beside it each layer's line,
//! in columns; the conversion's stages; and, done, what the image is: its ID and digest,
//! its platform and the others offered, its sizes, what it runs, where it is kept, and
//! what the pull took. A frame is drawn on each event from the daemon and every [`TICK`]
//! while something moves, over the last, at the terminal's size then, and only the lines
//! that changed are written.
//!
//! Off a terminal, or without colour, none of this runs: the client prints `docker
//! pull`'s lines as they come.

use std::io::Write as _;
use std::time::Duration;

use shards_ipc::Progress;
use shards_tui::bar::{self, Fill};
use shards_tui::canvas::Canvas;
use shards_tui::frame::Frame;
use shards_tui::layout;
use shards_tui::motion::{self, Clock};
use shards_tui::rate::Rate;
use shards_tui::text;
use shards_tui::tokens::{self, Paint, Rgb};

/// Between frames: 12 a second, slow enough to cost nothing, fast enough to read as
/// motion (the site paints its studies at 30).
pub const TICK: Duration = Duration::from_millis(83);

/// What the display is told: the daemon's events, and text to print below the frame.
pub enum Shown {
    Progress(Progress),
    Out(Vec<u8>),
    Err(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Waiting,
    Arriving,
    /// Already stored.
    Here,
    Verified,
    Unpacking,
    Unpacked,
}

struct Layer {
    digest: String,
    size: u64,
    got: u64,
    /// How much of it the plate shows arrived, eased toward `got`.
    shown: f64,
    state: State,
    /// Whether it was stored before the pull: shared with another image.
    had: bool,
    /// For a push: the repository the registry mounted it from.
    mounted: Option<String>,
}

/// The terminal's rows and columns now.
pub type Size = (usize, usize);

/// A line being made: its text, with colours, and its visible width.
#[derive(Default)]
struct Line {
    s: String,
    w: usize,
}

impl Line {
    fn new() -> Line {
        Line::default()
    }

    fn pad(&mut self, n: usize) -> &mut Line {
        self.s.extend(std::iter::repeat_n(' ', n));
        self.w += n;
        self
    }

    fn put(&mut self, p: &Paint, c: Rgb, text: &str) -> &mut Line {
        p.fg(&mut self.s, c);
        self.s.push_str(text);
        self.w += text.chars().count();
        self
    }

    /// `text` at `alpha` of 255 over the page: revealed by degrees.
    fn fade(&mut self, p: &Paint, c: Rgb, alpha: u8, text: &str) -> &mut Line {
        p.fg_over(&mut self.s, c, alpha);
        self.s.push_str(text);
        self.w += text.chars().count();
        self
    }

    fn bold(&mut self, p: &Paint, on: bool) -> &mut Line {
        p.bold(&mut self.s, on);
        self
    }

    fn done(self) -> (String, usize) {
        (self.s, self.w)
    }
}

/// A pull, as it stands, and how to draw it.
pub struct Pull {
    reference: String,
    repository: String,
    /// What is under way: a pull, or a push.
    verb: &'static str,
    layers: Vec<Layer>,
    building: bool,
    /// The pull's end: what the reference resolved to, whether nothing was new, whether
    /// it boots here; and when it ended.
    done: Option<(String, bool, bool)>,
    done_at: Option<f64>,
    facts: Vec<(String, String)>,
    clock: Clock,
    rate: Rate,
    /// When bytes first came, and when they stopped.
    began_at: Option<f64>,
    finished_at: Option<f64>,
    /// The last frame's time, for easing.
    last_t: f64,
    paint: Paint,
    ascii: bool,
    mark: Canvas,
    frame: Frame,
}

impl Pull {
    pub fn new(paint: Paint, east_asian: bool, reduced: bool) -> Pull {
        Pull {
            reference: String::new(),
            verb: "pull",
            repository: String::new(),
            layers: Vec::new(),
            building: false,
            done: None,
            done_at: None,
            facts: Vec::new(),
            clock: Clock::new(reduced),
            rate: Rate::new(),
            began_at: None,
            finished_at: None,
            last_t: 0.0,
            paint,
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

    fn fact(&self, name: &str) -> Option<&str> {
        self.facts
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    pub fn apply(&mut self, event: Progress) {
        let t = self.clock.t();
        let mut set = |d: &str, f: &dyn Fn(&mut Layer)| {
            if let Some(l) = self.layers.iter_mut().find(|l| l.digest == d) {
                f(l);
            }
        };
        match event {
            Progress::Pulling {
                reference,
                repository,
            } => {
                self.reference = reference;
                self.repository = repository;
            }
            Progress::Pushing {
                reference,
                repository,
            } => {
                self.verb = "push";
                self.began_at.get_or_insert(t);
                self.reference = reference;
                self.repository = repository;
            }
            Progress::Mounted(d, from) => {
                if let Some(l) = self.layers.iter_mut().find(|l| l.digest == d) {
                    l.state = State::Here;
                    l.got = l.size;
                    l.mounted = Some(from);
                }
            }
            Progress::Layers(layers) => {
                self.layers = layers
                    .into_iter()
                    .map(|(digest, size)| Layer {
                        digest,
                        size,
                        got: 0,
                        shown: 0.0,
                        state: State::Waiting,
                        had: false,
                        mounted: None,
                    })
                    .collect();
            }
            Progress::Have(d) => set(&d, &|l| {
                l.state = State::Here;
                l.had = true;
                l.got = l.size;
            }),
            Progress::Bytes(d, n) => {
                self.began_at.get_or_insert(t);
                set(&d, &|l| {
                    l.state = State::Arriving;
                    l.got = n;
                });
            }
            Progress::Verified(d) => set(&d, &|l| {
                l.state = State::Verified;
                l.got = l.size;
            }),
            Progress::Building => {
                self.building = true;
            }
            Progress::Unpacking(i) => {
                for l in &mut self.layers {
                    if l.state == State::Unpacking {
                        l.state = State::Unpacked;
                    }
                }
                if let Some(l) = self.layers.get_mut(i) {
                    l.state = State::Unpacking;
                    l.got = l.size;
                }
            }
            Progress::Facts(facts) => self.facts = facts,
            Progress::Done {
                digest,
                unchanged,
                bootable,
            } => {
                self.building = false;
                for l in &mut self.layers {
                    if bootable || l.state == State::Unpacking {
                        l.state = if bootable {
                            State::Unpacked
                        } else {
                            State::Verified
                        };
                    }
                    l.got = l.size;
                }
                self.done = Some((digest, unchanged, bootable));
                self.done_at = Some(t);
            }
        }
        if self
            .layers
            .iter()
            .all(|l| l.state != State::Waiting && l.state != State::Arriving)
            && !self.layers.is_empty()
        {
            self.finished_at.get_or_insert(t);
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
        let dt = (t - self.last_t).clamp(0.0, 0.5);
        self.last_t = t;
        for l in &mut self.layers {
            let truth = if l.size > 0 {
                l.got as f64 / l.size as f64
            } else {
                1.0
            };
            l.shown = if self.clock.reduced() {
                truth
            } else {
                motion::follow(l.shown, truth, 9.0, dt)
            };
        }
        shards_tui::mark::draw(&mut self.mark, t);
        let lines = self.compose(rows, cols, t);
        self.frame.begin();
        for (line, width) in &lines {
            self.frame.put(line, *width);
        }
        // Only the lines that changed are written; nothing, if none did.
        self.frame.end(cols, out);
    }

    /// Whether anything on screen moves by itself. When nothing does, frames come only
    /// with events.
    pub fn moving(&self, size: Size) -> bool {
        if self.clock.reduced() {
            return false;
        }
        // Done, the facts are revealed, and then all holds still.
        match self.done_at {
            Some(at) => self.clock.t() - at < REVEAL + 0.5,
            None => size.1 >= 40 || self.building || self.layers.iter().any(|l| l.state == State::Arriving),
        }
    }

    /// What follows is printed below the frame.
    pub fn leave(&mut self) {
        self.frame.leave();
    }

    /// Draws the last frames, the facts revealed whole if the pull is done, and leaves
    /// the frame for what follows; a pull that ended without them is left as it stands.
    fn settle(&mut self, size: &impl Fn() -> Size, out: &mut impl std::io::Write) {
        while self.done_at.is_some() && self.moving(size()) {
            std::thread::sleep(TICK);
            self.draw(size(), out);
        }
        self.draw(size(), out);
        self.leave();
    }

    /// The frame's lines, each with its visible width: never wider than `cols`, never
    /// more than `rows - 1`, so it redraws in place.
    fn compose(&mut self, rows: usize, cols: usize, t: f64) -> Vec<(String, usize)> {
        let mut lines: Vec<(String, usize)> = Vec::new();
        lines.extend(self.header(cols, rows, t));
        lines.push((String::new(), 0));
        // The conversion's stages, under the title and over the layers.
        let stages = self.stages(cols, t);
        if stages.1 > 0 {
            lines.push(stages);
            lines.push((String::new(), 0));
        }
        // The facts, once done, are what matters most: room is kept for them first.
        let facts = if self.done.is_some() {
            self.summary(cols, t)
        } else {
            Vec::new()
        };
        let room = rows
            .saturating_sub(1)
            .saturating_sub(lines.len() + facts.len() + usize::from(!facts.is_empty()));
        lines.extend(self.body(cols, room.max(1), t));
        if !facts.is_empty() {
            lines.push((String::new(), 0));
            lines.extend(facts);
        }
        // Never taller than the screen: a frame scrolled past its top cannot be redrawn.
        lines.truncate(rows.saturating_sub(1));
        lines
    }

    /// The head: the mark, where there is room, beside the brand and the command in
    /// the site's tracked capitals, what is pulled, and how it goes.
    fn header(&self, cols: usize, rows: usize, t: f64) -> Vec<(String, usize)> {
        let p = &self.paint;
        let with_mark = cols >= 56 && rows >= 14;
        // The head sits at the left edge, the body indented under it.
        let indent = if with_mark { 10 } else { 0 };
        let room = cols.saturating_sub(indent + 1);
        let mut info: Vec<(String, usize)> = Vec::new();
        {
            // The brand in the eyebrow's letters, the prism moving through them slowly.
            let brand = text::eyebrow("shards");
            let mut l = Line::new();
            if room >= brand.chars().count() {
                l.bold(p, true);
                let n = brand.chars().count().max(2) - 1;
                for (k, ch) in brand.chars().enumerate() {
                    let c = if self.clock.reduced() {
                        tokens::prism(k as f64 / n as f64)
                    } else {
                        tokens::prism_current(k as f64 / n as f64, t, 14.0)
                    };
                    l.put(p, c, &ch.to_string());
                }
                l.bold(p, false);
                let brow = text::eyebrow(self.verb);
                if room >= l.w + 5 + brow.chars().count() {
                    l.put(p, tokens::SUBTLE, "  ·  ").put(p, tokens::EYEBROW, &brow);
                }
            }
            info.push(l.done());
        }
        {
            let name = layout::familiar(&self.reference);
            let name = if name.is_empty() { "…" } else { name };
            let mut l = Line::new();
            l.bold(p, true)
                .put(p, tokens::BRIGHT, &layout::clip(name, room))
                .bold(p, false);
            info.push(l.done());
        }
        let registry = self.reference.split('/').next().unwrap_or_default();
        let platform = self.fact("platform").unwrap_or("");
        let (got, all) = self.total();
        let n = self.layers.len();
        if n > 0 {
            let layers = if n == 1 { "layer" } else { "layers" };
            let forms = [
                format!(
                    "{registry} · {n} {layers} · {}{}",
                    text::bytes(all),
                    if platform.is_empty() {
                        String::new()
                    } else {
                        format!(" · {platform}")
                    }
                ),
                format!("{n} {layers} · {}", text::bytes(all)),
                format!("{n}L · {}", text::bytes_short(all)),
            ];
            let form = forms
                .iter()
                .find(|f| f.chars().count() <= room)
                .cloned()
                .unwrap_or_default();
            let mut l = Line::new();
            l.put(p, tokens::MUTED, &form);
            info.push(l.done());
        }
        // How it goes: how much, how fast, what is left, its recent history.
        if self.done.is_none() && all > 0 && got < all {
            let r = self.rate.bytes_per_second();
            let eta = self.rate.eta(all.saturating_sub(got));
            let mut l = Line::new();
            l.put(
                p,
                tokens::FOREGROUND,
                &format!("{:>3.0}%", got as f64 * 100.0 / all as f64),
            );
            let rest = [
                format!(
                    "  {}  {}",
                    text::rate(r),
                    eta.map(|s| format!("{} left", text::duration(s)))
                        .unwrap_or_default()
                ),
                format!("  {}", text::rate(r)),
                String::new(),
            ];
            let rest = rest
                .iter()
                .find(|f| f.chars().count() + 4 <= room)
                .cloned()
                .unwrap_or_default();
            l.put(p, tokens::MUTED, &rest);
            let spark = room.saturating_sub(l.w + 2).min(12);
            if spark >= 4 {
                l.pad(2);
                self.rate.sparkline(&mut l.s, p, spark);
                l.w += spark;
            }
            info.push(l.done());
        } else if self.building {
            let mut l = Line::new();
            let words = "building its microVM";
            for (k, ch) in words.chars().enumerate() {
                let at = k as f64 / words.chars().count() as f64;
                let lit = motion::glint(at, motion::travel(t, 0.4, 0.6, 0.0), 0.1);
                l.put(
                    p,
                    tokens::mix(tokens::MUTED, tokens::LAVENDER, lit * 0.8),
                    &ch.to_string(),
                );
            }
            info.push(l.done());
        }
        let mark_rows = if with_mark { self.mark.rows() } else { 0 };
        (0..info.len().max(mark_rows))
            .map(|i| {
                let mut l = Line::new();
                if with_mark {
                    if i < mark_rows {
                        self.mark.row(i, p, &mut l.s);
                        l.w += 8;
                    } else {
                        l.pad(8);
                    }
                    l.pad(2);
                }
                if let Some((s, w)) = info.get(i) {
                    l.s.push_str(s);
                    l.w += w;
                }
                l.done()
            })
            .collect()
    }

    /// The layers' lines, in `room` rows at most.
    fn body(&mut self, cols: usize, room: usize, t: f64) -> Vec<(String, usize)> {
        self.table(cols.saturating_sub(4), room, t)
            .into_iter()
            .map(|(s, w)| (format!("    {s}"), w + 4))
            .collect()
    }

    /// The layers' lines, in columns, `width` wide and `room` rows at most: the settled
    /// ones folded into a count when they do not all fit.
    fn table(&self, width: usize, room: usize, t: f64) -> Vec<(String, usize)> {
        let p = &self.paint;
        let settled = |l: &Layer| matches!(l.state, State::Here | State::Verified | State::Unpacked);
        let fold = self.layers.len() > room && self.layers.iter().any(settled);
        let mut out = Vec::new();
        if fold {
            let (k, bytes) = self
                .layers
                .iter()
                .filter(|l| settled(l))
                .fold((0, 0u64), |(k, b), l| (k + 1, b + l.size));
            let mut l = Line::new();
            l.put(p, tokens::SAGE, if self.ascii { "*" } else { "●" });
            l.put(
                p,
                tokens::MUTED,
                &layout::clip(
                    &format!("  {k} layers settled  {}", text::bytes(bytes)),
                    width.saturating_sub(1),
                ),
            );
            out.push(l.done());
        }
        // Column widths: the size column as wide as the widest size, the state column
        // as wide as its widest word.
        let size_w = self
            .layers
            .iter()
            .map(|l| text::bytes(l.size).len())
            .max()
            .unwrap_or(0);
        const STATE_W: usize = 9;
        let id_w = if width >= 12 + 2 + 8 + 2 + size_w + 2 + STATE_W + 2 {
            12
        } else {
            6
        };
        let candidates = self.layers.iter().filter(|l| !fold || !settled(l)).count();
        let mut slots = room.saturating_sub(usize::from(fold));
        // More to come than there are rows: the last row says how many.
        let more = candidates.saturating_sub(slots);
        let more = if more > 0 {
            slots = slots.saturating_sub(1);
            candidates - slots
        } else {
            0
        };
        let shown = self
            .layers
            .iter()
            .enumerate()
            .filter(|(_, l)| !fold || !settled(l))
            .take(slots);
        for (i, layer) in shown {
            // Under way, amber; here, grey; verified and in the disk, sage; unpacking,
            // lavender: the site's state colours.
            let tone = match layer.state {
                State::Waiting => tokens::SUBTLE,
                State::Arriving => tokens::AMBER,
                State::Here => tokens::MUTED,
                State::Verified | State::Unpacked => tokens::SAGE,
                State::Unpacking => tokens::LAVENDER,
            };
            let mut l = Line::new();
            let dot = match layer.state {
                State::Waiting => {
                    if self.ascii {
                        "."
                    } else {
                        "·"
                    }
                }
                State::Arriving => {
                    if self.ascii {
                        "o"
                    } else {
                        "◌"
                    }
                }
                State::Unpacking => {
                    if self.ascii {
                        "#"
                    } else {
                        "◆"
                    }
                }
                _ => {
                    if self.ascii {
                        "*"
                    } else {
                        "●"
                    }
                }
            };
            let dot_c = match layer.state {
                State::Arriving => tokens::mix(
                    tone,
                    tokens::BRIGHT,
                    0.35 * motion::breath(t, 1.6, i as f64 * 0.7),
                ),
                _ => tone,
            };
            l.put(p, dot_c, dot);
            let hex = layer
                .digest
                .split_once(':')
                .map_or(layer.digest.as_str(), |(_, h)| h);
            l.pad(2).put(p, tokens::SUBTLE, hex.get(..id_w).unwrap_or(hex));
            let word = match layer.state {
                State::Waiting => "waiting".to_string(),
                State::Arriving if layer.size > 0 => {
                    format!("{:>3.0}%", layer.got as f64 * 100.0 / layer.size as f64)
                }
                State::Arriving if self.verb == "push" => "uploading".to_string(),
                State::Arriving => "arriving".to_string(),
                State::Here if layer.mounted.is_some() => "mounted".to_string(),
                State::Here if self.verb == "push" => "there".to_string(),
                State::Here => "stored".to_string(),
                State::Verified if self.verb == "push" => "pushed".to_string(),
                State::Verified => "verified".to_string(),
                State::Unpacking => "unpacking".to_string(),
                State::Unpacked => "on disk".to_string(),
            };
            let bar_w = width
                .saturating_sub(1 + 2 + id_w + 2 + 2 + size_w + 2 + STATE_W)
                .min(40);
            if bar_w >= 6 {
                l.pad(2);
                let fill = match layer.state {
                    State::Waiting => Fill::Waiting,
                    State::Arriving if layer.size == 0 => Fill::Unknown,
                    State::Arriving => Fill::Going(layer.shown),
                    _ => Fill::Done,
                };
                bar::draw(&mut l.s, p, bar_w, fill, t, i as f64 * 0.37);
                l.w += bar_w;
            }
            let size = text::bytes(layer.size);
            if l.w + 2 + size_w + 2 + STATE_W <= width {
                l.pad(2 + size_w - size.len()).put(p, tokens::MUTED, &size);
                let c = match layer.state {
                    State::Verified | State::Unpacked => tokens::SAGE,
                    State::Unpacking => tokens::LAVENDER,
                    State::Arriving => tokens::FOREGROUND,
                    _ => tokens::SUBTLE,
                };
                l.pad(2).put(p, c, &word);
            }
            out.push(l.done());
        }
        if more > 0 && slots < room {
            let mut l = Line::new();
            l.put(p, tokens::SUBTLE, if self.ascii { "..." } else { "…" });
            l.put(
                p,
                tokens::MUTED,
                &layout::clip(&format!("  {more} more"), width.saturating_sub(3)),
            );
            out.push(l.done());
        }
        out
    }

    /// The conversion's stages, each lit as it goes: fetched, verified, unpacked, the
    /// disk written, the microVM ready.
    fn stages(&self, cols: usize, t: f64) -> (String, usize) {
        let p = &self.paint;
        let all = !self.layers.is_empty();
        let fetched = all
            && self
                .layers
                .iter()
                .all(|l| !matches!(l.state, State::Waiting | State::Arriving));
        let fetching = self.layers.iter().any(|l| l.state == State::Arriving);
        let unpacked = self.done.is_some();
        let bootable = self.done.as_ref().is_some_and(|d| d.2);
        // (name, short, done, going)
        let pulled = [
            ("fetch", "fetch", fetched, fetching),
            ("verify", "check", fetched, fetching),
            ("unpack", "unpack", unpacked, self.building),
            ("erofs disk", "disk", unpacked, self.building),
            ("microvm", "vm", bootable, false),
        ];
        // A push: each layer checked for, mounted or uploaded; then the manifest, by tag.
        let started = all && self.layers.iter().any(|l| l.state != State::Waiting);
        let pushed = [
            ("check", "check", started, all && !started),
            ("mount", "mount", fetched, started && !fetched),
            ("upload", "upload", fetched, fetching),
            ("manifest", "manifest", unpacked, fetched && !unpacked),
            ("tag", "tag", unpacked, false),
        ];
        let stages = if self.verb == "push" { pushed } else { pulled };
        let long: usize = stages.iter().map(|s| s.0.len()).sum::<usize>() + 2 * 5 + 5 * 4 + 2;
        let use_long = long <= cols;
        let mut l = Line::new();
        // Under the layers' dots: their column.
        l.pad(4);
        for (k, (name, short, done, going)) in stages.iter().enumerate() {
            if k > 0 {
                // The joint: a light running along it into the stage that goes.
                let joint = if self.ascii { "-->" } else { "──▸" };
                for (j, ch) in joint.chars().enumerate() {
                    let lit = if *going {
                        motion::glint(j as f64 / 3.0, motion::travel(t, 1.2, 0.2, 0.0), 0.25)
                    } else {
                        0.0
                    };
                    let base = if *done { tokens::EDGE } else { tokens::LINE };
                    l.put(p, tokens::mix(base, tokens::LAVENDER, lit), &ch.to_string());
                }
                l.pad(1);
            }
            let (glyph, c) = if *done {
                (if self.ascii { "*" } else { "◆" }, tokens::SAGE)
            } else if *going {
                (
                    if self.ascii { "o" } else { "◈" },
                    tokens::mix(tokens::LAVENDER, tokens::BRIGHT, motion::breath(t, 1.4, k as f64)),
                )
            } else {
                (if self.ascii { "." } else { "◇" }, tokens::FAINT)
            };
            l.put(p, c, glyph).pad(1);
            let word = if use_long { name } else { short };
            l.put(
                p,
                if *done || *going {
                    tokens::FOREGROUND
                } else {
                    tokens::SUBTLE
                },
                word,
            );
            l.pad(1);
        }
        if l.w > cols {
            return (String::new(), 0);
        }
        l.done()
    }

    /// What the image is, once pulled: a line that says so, then its facts in columns,
    /// revealed one after another.
    fn summary(&self, cols: usize, t: f64) -> Vec<(String, usize)> {
        let p = &self.paint;
        let Some((digest, unchanged, bootable)) = &self.done else {
            return Vec::new();
        };
        let since = self.done_at.map_or(REVEAL, |at| t - at);
        let alpha = |k: usize| -> u8 {
            if self.clock.reduced() {
                return 255;
            }
            let u = ((since - k as f64 * 0.045) / 0.4).clamp(0.0, 1.0);
            (motion::expo_out(u) * 255.0).round() as u8
        };
        let mut out = Vec::new();
        let name = layout::familiar(&self.reference);
        let said = match (unchanged, bootable) {
            _ if self.verb == "push" && *unchanged => {
                ["Already there.".to_string(), "Already there.".to_string()]
            }
            _ if self.verb == "push" => [
                format!("Pushed. {name} is in its registry."),
                "Pushed.".to_string(),
            ],
            (true, _) => ["Up to date.".to_string(), "Up to date.".to_string()],
            (false, true) => [format!("Ready. {name} is a microVM now."), "Ready.".to_string()],
            (false, false) => [
                format!("Stored. {name} is for another platform."),
                "Stored.".to_string(),
            ],
        };
        let words = said
            .iter()
            .find(|w| w.chars().count() + 2 <= cols)
            .cloned()
            .unwrap_or_default();
        // In the column the stages and the layers' dots are in, under the head.
        let mut l = Line::new();
        l.pad(4)
            .bold(p, true)
            .fade(p, tokens::BRIGHT, alpha(0), &words)
            .bold(p, false);
        out.push(l.done());
        out.push((String::new(), 0));
        let facts = self.fact_rows(digest);
        // A fact a row, labels in the eyebrow's grey.
        const LABEL: usize = 10;
        let value_w = cols.saturating_sub(6 + LABEL);
        for (k, (label, value, tone)) in facts.iter().enumerate() {
            let a = alpha(k + 1);
            let mut l = Line::new();
            l.pad(4);
            l.fade(p, tokens::EYEBROW, a, &format!("{label:<LABEL$}"));
            l.fade(p, *tone, a, &layout::clip(value, value_w));
            out.push(l.done());
        }
        out
    }

    /// The facts to show: label, value, and its colour.
    fn fact_rows(&self, digest: &str) -> Vec<(&'static str, String, Rgb)> {
        if self.verb == "push" {
            return self.push_rows(digest);
        }
        let mut rows = Vec::new();
        let short = |d: &str| -> String {
            let hex = d.split_once(':').map_or(d, |(_, h)| h);
            hex.get(..12).unwrap_or(hex).to_string()
        };
        if let Some(id) = self.fact("id") {
            rows.push(("id", short(id), tokens::BRIGHT));
        }
        rows.push(("digest", digest.to_string(), tokens::FOREGROUND));
        if let Some(platform) = self.fact("platform") {
            let others = self
                .fact("platforms")
                .map(|all| all.split(' ').filter(|o| *o != platform).count())
                .unwrap_or(0);
            let v = if others > 0 {
                format!("{platform}  +{others} more offered")
            } else {
                platform.to_string()
            };
            rows.push(("platform", v, tokens::FOREGROUND));
        }
        if let Some(created) = self.fact("created") {
            rows.push(("created", created_words(created), tokens::FOREGROUND));
        }
        let all: u64 = self.layers.iter().map(|l| l.size).sum();
        let (here, here_bytes) = self
            .layers
            .iter()
            .filter(|l| l.had)
            .fold((0, 0u64), |(k, b), l| (k + 1, b + l.size));
        let n = self.layers.len();
        let mut layers = format!("{n} · {} compressed", text::bytes(all));
        if here > 0 {
            layers.push_str(&format!(" · {here} shared ({})", text::bytes(here_bytes)));
        }
        rows.push(("layers", layers, tokens::FOREGROUND));
        if let Some(bytes) = self.fact("rootfs_bytes").and_then(|b| b.parse::<u64>().ok()) {
            rows.push((
                "microvm",
                format!("{} EROFS disk · boots from a template", text::bytes(bytes)),
                tokens::LAVENDER,
            ));
        }
        let runs = [self.fact("entrypoint"), self.fact("cmd")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        if !runs.is_empty() {
            rows.push(("runs", runs, tokens::FOREGROUND));
        }
        let user = self.fact("user").unwrap_or("root");
        let dir = self.fact("workdir").unwrap_or("/");
        rows.push(("as", format!("{user} in {dir}"), tokens::FOREGROUND));
        if let Some(ports) = self.fact("ports") {
            rows.push(("ports", ports.to_string(), tokens::FOREGROUND));
        }
        if let Some(volumes) = self.fact("volumes") {
            rows.push(("volumes", volumes.to_string(), tokens::FOREGROUND));
        }
        if let Some(env) = self.fact("env").filter(|e| *e != "0") {
            let noun = if env == "1" { "variable" } else { "variables" };
            rows.push(("env", format!("{env} {noun}"), tokens::FOREGROUND));
        }
        let titled = [self.fact("title"), self.fact("version")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        if !titled.is_empty() {
            rows.push(("title", titled, tokens::FOREGROUND));
        }
        if let Some(project) = self.fact("source") {
            rows.push(("project", project.to_string(), tokens::TEAL));
        }
        if let Some(licenses) = self.fact("licenses") {
            rows.push(("license", licenses.to_string(), tokens::FOREGROUND));
        }
        if let Some(k) = self.fact("attestations").filter(|k| *k != "0") {
            let noun = if k == "1" { "attestation" } else { "attestations" };
            rows.push((
                "attested",
                format!("{k} {noun} kept: provenance, SBOM"),
                tokens::SAGE,
            ));
        }
        if let Some(store) = self.fact("store") {
            let free = self
                .fact("free")
                .and_then(|f| f.parse::<u64>().ok())
                .map(|f| format!(" · {} free", text::bytes(f)))
                .unwrap_or_default();
            rows.push(("stored", format!("{}{free}", home_relative(store)), tokens::MUTED));
        }
        let fetched: u64 = self.layers.iter().filter(|l| !l.had).map(|l| l.size).sum();
        if let (Some(a), Some(b)) = (self.began_at, self.finished_at)
            && fetched > 0
            && b > a
        {
            rows.push((
                "fetched",
                format!(
                    "{} in {} · {}",
                    text::bytes(fetched),
                    text::duration(b - a),
                    text::rate(fetched as f64 / (b - a))
                ),
                tokens::MUTED,
            ));
        }
        // What was pulled, by its whole name: the CLI's last line.
        if let Some(reference) = self.fact("reference") {
            rows.push(("source", reference.to_string(), tokens::FOREGROUND));
        }
        rows
    }
}

impl Pull {
    /// A push's facts: where it went, its digest and size, and what each layer took.
    fn push_rows(&self, digest: &str) -> Vec<(&'static str, String, Rgb)> {
        let mut rows = Vec::new();
        rows.push(("digest", digest.to_string(), tokens::FOREGROUND));
        if let Some(size) = self.fact("size").and_then(|s| s.parse::<u64>().ok()) {
            rows.push(("manifest", text::bytes(size), tokens::FOREGROUND));
        }
        let all: u64 = self.layers.iter().map(|l| l.size).sum();
        let (mut sent, mut sent_bytes, mut mounted, mut there) = (0, 0u64, 0, 0);
        for l in &self.layers {
            match (l.state, &l.mounted) {
                (State::Verified, _) => {
                    sent += 1;
                    sent_bytes += l.size;
                }
                (State::Here, Some(_)) => mounted += 1,
                (State::Here, None) => there += 1,
                _ => {}
            }
        }
        let n = self.layers.len();
        rows.push((
            "layers",
            format!(
                "{n} · {} · {sent} uploaded, {mounted} mounted, {there} there already",
                text::bytes(all)
            ),
            tokens::FOREGROUND,
        ));
        if let Some(from) = self.layers.iter().find_map(|l| l.mounted.clone()) {
            rows.push(("mounted", format!("from {from}"), tokens::TEAL));
        }
        // How long, as the daemon timed it: what the client hears comes in bursts.
        let took = self
            .fact("elapsed_ms")
            .and_then(|m| m.parse::<f64>().ok())
            .map(|m| m / 1000.0)
            .filter(|s| *s > 0.0);
        if let Some(took) = took
            && sent_bytes > 0
        {
            rows.push((
                "uploaded",
                format!(
                    "{} in {} · {}",
                    text::bytes(sent_bytes),
                    text::duration(took),
                    text::rate(sent_bytes as f64 / took)
                ),
                tokens::MUTED,
            ));
        }
        if let Some(index) = self.fact("partial") {
            let short = index.split_once(':').map_or(index, |(_, h)| h);
            rows.push((
                "platform",
                format!(
                    "this platform's image alone: index {} names others not here",
                    short.get(..12).unwrap_or(short)
                ),
                tokens::AMBER,
            ));
        }
        rows.push(("target", self.reference.clone(), tokens::FOREGROUND));
        rows
    }
}

/// How long the facts take to be revealed.
const REVEAL: f64 = 1.2;

/// `path` with this user's home as `~`.
fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) => format!("~{rest}"),
            None => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// An RFC 3339 time as a date and how long ago: `2026-09-12 · 22 days ago`.
fn created_words(rfc3339: &str) -> String {
    let date = rfc3339.get(..10).unwrap_or(rfc3339);
    let days = |d: &str| -> Option<i64> {
        let y: i64 = d.get(0..4)?.parse().ok()?;
        let m: i64 = d.get(5..7)?.parse().ok()?;
        let day: i64 = d.get(8..10)?.parse().ok()?;
        // Days from the civil date (Howard Hinnant's days_from_civil).
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(era * 146_097 + doe - 719_468)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    match days(date) {
        Some(then) if now >= then => {
            let ago = now - then;
            let words = match ago {
                0 => "today".to_string(),
                1 => "yesterday".to_string(),
                2..=60 => format!("{ago} days ago"),
                61..=730 => format!("{} months ago", ago / 30),
                _ => format!("{} years ago", ago / 365),
            };
            format!("{date} · {words}")
        }
        _ => date.to_string(),
    }
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
                    pull.settle(&size, &mut stdout);
                    started = false;
                }
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            Ok(Shown::Err(bytes)) => {
                if started {
                    pull.settle(&size, &mut stdout);
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
                    pull.settle(&size, &mut stdout);
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
        let mut p = Pull::new(Paint::new(true), false, true);
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

    fn finish(p: &mut Pull) {
        p.apply(Progress::Facts(vec![
            ("id".into(), format!("sha256:{:064x}", 0xabc)),
            ("platform".into(), "linux/arm64".into()),
            ("platforms".into(), "linux/amd64 linux/arm64 linux/s390x".into()),
            ("created".into(), "2026-09-12T10:00:00Z".into()),
            ("rootfs_bytes".into(), "85300000".into()),
            ("cmd".into(), "python3".into()),
            ("store".into(), "/home/u/.shards/images".into()),
            ("free".into(), "412000000000".into()),
        ]));
        p.apply(Progress::Done {
            digest: format!("sha256:{:064x}", 7),
            unchanged: false,
            bootable: true,
        });
    }

    #[test]
    fn every_line_fits_every_terminal() {
        for cols in [20usize, 32, 48, 56, 80, 100, 120, 160, 240] {
            for rows in [6usize, 14, 24, 40] {
                let mut p = pulled(6);
                p.apply(Progress::Have(format!("sha256:{:064x}", 0)));
                p.apply(Progress::Bytes(format!("sha256:{:064x}", 1), 700_000));
                p.apply(Progress::Verified(format!("sha256:{:064x}", 2)));
                for phase in 0..3 {
                    if phase == 1 {
                        p.apply(Progress::Building);
                        p.apply(Progress::Unpacking(1));
                    }
                    if phase == 2 {
                        finish(&mut p);
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
        let text = |p: &mut Pull| -> String {
            p.compose(48, 140, 0.0)
                .iter()
                .map(|(l, _)| seen(l) + "\n")
                .collect()
        };
        let now = text(&mut p);
        assert!(now.contains("P U L L"), "{now}");
        assert!(now.contains("python:3.13"), "{now}");
        assert!(now.contains("3 layers · 6.00 MB"), "{now}");
        assert!(now.contains("S H A R D S  ·  P U L L"), "{now}");
        assert!(now.contains("stored"), "{now}");
        assert!(now.contains(" 25%"), "{now}");
        assert!(now.contains("erofs disk"), "{now}");
        p.apply(Progress::Verified(format!("sha256:{:064x}", 1)));
        p.apply(Progress::Verified(format!("sha256:{:064x}", 2)));
        p.apply(Progress::Building);
        p.apply(Progress::Unpacking(0));
        let now = text(&mut p);
        assert!(now.contains("unpacking"), "{now}");
        assert!(now.contains("building its microVM"), "{now}");
        finish(&mut p);
        let now = text(&mut p);
        assert!(now.contains("Ready. python:3.13 is a microVM now."), "{now}");
        assert!(now.contains(&format!("id        {:012x}", 0)), "{now}");
        assert!(now.contains("linux/arm64  +2 more offered"), "{now}");
        assert!(now.contains("2026-09-12 · "), "{now}");
        assert!(now.contains("85.3 MB EROFS disk"), "{now}");
        assert!(now.contains("412 GB free"), "{now}");
        assert!(now.contains("1 shared (1.00 MB)"), "{now}");
        assert!(now.contains("on disk"), "{now}");
    }

    #[test]
    fn sizes_and_states_stand_in_columns() {
        let mut p = pulled(3);
        p.apply(Progress::Verified(format!("sha256:{:064x}", 0)));
        p.apply(Progress::Bytes(format!("sha256:{:064x}", 1), 500_000));
        let table = p.table(90, 10, 0.0);
        let ends: Vec<usize> = table
            .iter()
            .map(|(l, _)| {
                let text = seen(l);
                let at = text.find(" MB").unwrap();
                text.char_indices().take_while(|(i, _)| *i < at + 3).count()
            })
            .collect();
        assert!(ends.windows(2).all(|w| w[0] == w[1]), "{ends:?}");
    }

    #[test]
    fn the_title_stays_at_the_top_however_many_layers_come() {
        for (layers, rows, cols) in [(60, 20, 100), (300, 14, 80), (8, 10, 60), (120, 40, 200)] {
            let mut p = pulled(layers);
            for i in 0..layers / 3 {
                p.apply(Progress::Verified(format!("sha256:{i:064x}")));
            }
            p.apply(Progress::Bytes(format!("sha256:{:064x}", layers / 2), 1));
            for done in [false, true] {
                if done {
                    finish(&mut p);
                }
                let lines = p.compose(rows, cols, 0.0);
                assert!(lines.len() < rows, "{layers} in {rows}: {}", lines.len());
                let text: Vec<String> = lines.iter().map(|(l, _)| seen(l)).collect();
                assert!(text[0].contains("S H A R D S"), "{layers} in {rows}: {text:?}");
                assert!(text[1].contains("python:3.13"), "{layers} in {rows}: {text:?}");
                if !done {
                    assert!(
                        text.iter().any(|l| l.contains(" more")),
                        "{layers} in {rows}: {text:?}"
                    );
                }
            }
        }
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

    #[test]
    fn dates_say_how_long_ago() {
        assert!(created_words("2001-02-03T04:05:06Z").starts_with("2001-02-03 · "));
        assert!(created_words("2001-02-03T04:05:06Z").ends_with("years ago"));
        assert_eq!(created_words("garbage"), "garbage");
    }
}
