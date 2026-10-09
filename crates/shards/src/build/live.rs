//! How a build looks on a colour terminal: shards' own, in pull's look. The head, then
//! each step as it goes: lavender while it runs, its last lines under it; sage once
//! done, with its time; rose where it failed, with why. Done, the image it made: its ID,
//! its names, how many steps and how long. Drawn over the last frame as each thing
//! happens, only the lines that changed; on stderr, where BuildKit's progress goes.

use std::collections::VecDeque;
use std::time::Instant;

use shards_tui::canvas::Canvas;
use shards_tui::frame::Frame;
use shards_tui::layout;
use shards_tui::motion;
use shards_tui::text;
use shards_tui::tokens::{self, Paint, Rgb};

/// The last lines of a running step that are shown under it.
const TAIL: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Done,
    Cached,
    Failed,
    Canceled,
}

struct Step {
    name: String,
    started: Instant,
    took: Option<f64>,
    state: State,
    tail: VecDeque<String>,
    /// A line not yet ended, continued by what comes next.
    partial: String,
}

/// A build as it stands, and how to draw it.
pub(super) struct Live {
    paint: Paint,
    began: Instant,
    steps: Vec<Step>,
    mark: Canvas,
    frame: Frame,
    /// What the export said: the image's ID and its names.
    id: Option<String>,
    names: Vec<String>,
    finished: bool,
}

impl Live {
    /// A live display, if stderr is a colour terminal.
    pub(super) fn new() -> Option<Live> {
        let paint = crate::cli::look::styled_err()?;
        Some(Live {
            paint,
            began: Instant::now(),
            steps: Vec::new(),
            mark: Canvas::new(8, 4),
            frame: Frame::new(),
            id: None,
            names: Vec::new(),
            finished: false,
        })
    }

    /// Step `index` (from 1) begins.
    pub(super) fn start(&mut self, name: &str) {
        self.steps.push(Step {
            name: name.to_string(),
            started: Instant::now(),
            took: None,
            state: State::Running,
            tail: VecDeque::new(),
            partial: String::new(),
        });
        self.draw();
    }

    /// Step `index` begins again, as progressui shows a vertex that runs again.
    pub(super) fn resume(&mut self, index: usize) {
        if let Some(s) = self.step(index) {
            s.state = State::Running;
            s.started = Instant::now();
            s.took = None;
        }
        self.draw();
    }

    fn step(&mut self, index: usize) -> Option<&mut Step> {
        self.steps.get_mut(index.checked_sub(1)?)
    }

    /// A line step `index` says of itself.
    pub(super) fn line(&mut self, index: usize, text: &str) {
        if let Some(rest) = text.strip_prefix("writing image ") {
            self.id = rest.strip_suffix(" done").map(str::to_string);
        }
        if let Some(rest) = text.strip_prefix("naming to ")
            && let Some(name) = rest.strip_suffix(" done")
        {
            self.names.push(name.to_string());
        }
        if let Some(s) = self.step(index) {
            push(&mut s.tail, text.to_string());
        }
        self.draw();
    }

    /// Bytes a step's command printed.
    pub(super) fn output(&mut self, index: usize, bytes: &[u8]) {
        if let Some(s) = self.step(index) {
            let text = String::from_utf8_lossy(bytes);
            let mut rest: &str = &text;
            while let Some(end) = rest.find('\n') {
                let (line, after) = rest.split_at(end);
                let whole = std::mem::take(&mut s.partial) + line;
                push(&mut s.tail, whole);
                rest = after.get(1..).unwrap_or_default();
            }
            s.partial.push_str(rest);
        }
        self.draw();
    }

    pub(super) fn done(&mut self, index: usize) {
        if let Some(s) = self.step(index) {
            s.state = State::Done;
            s.took = Some(s.started.elapsed().as_secs_f64());
            if s.name == "exporting to image" {
                self.finished = true;
            }
        }
        self.draw();
        if self.finished {
            self.frame.leave();
            // Every screen ends with an empty line.
            let _ = std::io::Write::write_all(&mut std::io::stderr(), b"\n");
        }
    }

    pub(super) fn error(&mut self, index: usize, message: &str) {
        if let Some(s) = self.step(index) {
            s.state = State::Failed;
            s.took = Some(s.started.elapsed().as_secs_f64());
            push(&mut s.tail, message.to_string());
        }
        self.draw();
    }

    /// A step the build cache answered.
    pub(super) fn cached(&mut self, index: usize) {
        if let Some(s) = self.step(index) {
            s.state = State::Cached;
            s.took = Some(0.0);
        }
        self.draw();
    }

    pub(super) fn canceled(&mut self, index: usize) {
        if let Some(s) = self.step(index) {
            s.state = State::Canceled;
        }
        self.draw();
    }

    /// The display left as it stands, for what follows.
    pub(super) fn leave(&mut self) {
        self.draw();
        self.frame.leave();
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"\n");
    }

    fn draw(&mut self) {
        let (rows, cols) = match crate::cli::look::err_size() {
            (0, _) | (_, 0) => (40, 80),
            (r, c) => (r, c),
        };
        let t = self.began.elapsed().as_secs_f64();
        shards_tui::mark::draw(&mut self.mark, t);
        let lines = self.compose(rows, cols, t);
        self.frame.begin();
        for (s, w) in &lines {
            self.frame.put(s, *w);
        }
        self.frame.end(cols, &mut std::io::stderr().lock());
    }

    fn compose(&self, rows: usize, cols: usize, t: f64) -> Vec<(String, usize)> {
        let p = &self.paint;
        let mut out: Vec<(String, usize)> = Vec::new();
        // The head: the mark beside the brand, the command, how it goes.
        let done = self.steps.iter().filter(|s| s.state == State::Done).count();
        let failed = self.steps.iter().any(|s| s.state == State::Failed);
        let mut info: Vec<(String, usize)> = Vec::new();
        {
            let mut l = Line::default();
            l.bold(p, true);
            let brand = text::eyebrow("shards");
            let n = brand.chars().count().max(2) - 1;
            for (k, ch) in brand.chars().enumerate() {
                l.put(
                    p,
                    tokens::prism_current(k as f64 / n as f64, t, 14.0),
                    &ch.to_string(),
                );
            }
            l.bold(p, false)
                .put(p, tokens::SUBTLE, "  ·  ")
                .put(p, tokens::EYEBROW, &text::eyebrow("build"));
            info.push(l.done());
        }
        {
            let mut l = Line::default();
            let words = if self.finished {
                format!("built in {}", text::duration(t))
            } else if failed {
                "failed".to_string()
            } else {
                format!("{done} of {} steps · {}", self.steps.len(), text::duration(t))
            };
            l.put(p, if failed { tokens::ROSE } else { tokens::MUTED }, &words);
            info.push(l.done());
        }
        let with_mark = cols >= 56 && rows >= 14;
        for i in 0..info.len().max(if with_mark { self.mark.rows() } else { 0 }) {
            let mut l = Line::default();
            if with_mark {
                if i < self.mark.rows() {
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
            out.push(l.done());
        }
        out.push((String::new(), 0));
        // The steps: the last that fit, each a line, a running one's tail under it.
        let summary = if self.finished { 4 + self.names.len() } else { 0 };
        let room = rows.saturating_sub(out.len() + 1 + summary);
        let mut body: Vec<(String, usize)> = Vec::new();
        for (i, s) in self.steps.iter().enumerate() {
            let (glyph, c): (&str, Rgb) = match s.state {
                State::Running => (
                    "◈",
                    tokens::mix(tokens::LAVENDER, tokens::BRIGHT, motion::breath(t, 1.4, i as f64)),
                ),
                State::Done => ("◆", tokens::SAGE),
                State::Cached => ("◆", tokens::SUBTLE),
                State::Failed => ("○", tokens::ROSE),
                State::Canceled => ("◇", tokens::SUBTLE),
            };
            let took = s
                .took
                .map(text::duration)
                .unwrap_or_else(|| text::duration(s.started.elapsed().as_secs_f64()));
            let mut l = Line::default();
            l.pad(4).put(p, c, glyph).pad(1);
            let room_name = cols.saturating_sub(4 + 2 + took.len() + 3);
            let name = layout::clip(&s.name, room_name);
            let nc = match s.state {
                State::Running => tokens::BRIGHT,
                State::Failed => tokens::ROSE,
                State::Canceled => tokens::SUBTLE,
                State::Done => tokens::FOREGROUND,
                State::Cached => tokens::SUBTLE,
            };
            l.put(p, nc, &name);
            l.to(cols.saturating_sub(took.len() + 1))
                .put(p, tokens::SUBTLE, &took);
            body.push(l.done());
            if matches!(s.state, State::Running | State::Failed) {
                let tail: Vec<&String> = s
                    .tail
                    .iter()
                    .chain(std::iter::once(&s.partial).filter(|p| !p.is_empty()))
                    .collect();
                for line in tail.iter().rev().take(TAIL).rev() {
                    let mut l = Line::default();
                    l.pad(8).put(
                        p,
                        if s.state == State::Failed {
                            tokens::ROSE
                        } else {
                            tokens::SUBTLE
                        },
                        &layout::clip(line, cols.saturating_sub(9)),
                    );
                    body.push(l.done());
                }
            }
        }
        // The newest steps where all do not fit.
        let skip = body.len().saturating_sub(room);
        out.extend(body.into_iter().skip(skip));
        if self.finished {
            out.push((String::new(), 0));
            let mut l = Line::default();
            l.pad(4)
                .bold(p, true)
                .put(p, tokens::BRIGHT, "Built.")
                .bold(p, false);
            out.push(l.done());
            out.push((String::new(), 0));
            let mut row = |label: &str, value: &str, c: Rgb| {
                let mut l = Line::default();
                l.pad(4).put(p, tokens::EYEBROW, &format!("{label:<10}"));
                l.put(p, c, &layout::clip(value, cols.saturating_sub(15)));
                out.push(l.done());
            };
            if let Some(id) = &self.id {
                row("id", id, tokens::FOREGROUND);
            }
            for name in &self.names {
                row("name", shards_tui::layout::familiar(name), tokens::BRIGHT);
            }
            // The Dockerfile's instructions, `[n/N]`, apart from the build's own steps.
            let instructions = self
                .steps
                .iter()
                .filter(|s| s.name.starts_with('[') && !s.name.starts_with("[internal]"))
                .count();
            row(
                "steps",
                &format!(
                    "{instructions} {} · {} steps in all · {}",
                    if instructions == 1 {
                        "instruction"
                    } else {
                        "instructions"
                    },
                    self.steps.len(),
                    text::duration(t)
                ),
                tokens::MUTED,
            );
        }
        out.truncate(rows.saturating_sub(1));
        out
    }
}

/// `line` onto `tail`, which keeps only the last [`TAIL`].
fn push(tail: &mut VecDeque<String>, line: String) {
    tail.push_back(line);
    while tail.len() > TAIL {
        tail.pop_front();
    }
}

/// A line being made: its text, with colours, and its visible width.
#[derive(Default)]
struct Line {
    s: String,
    w: usize,
}

impl Line {
    fn pad(&mut self, n: usize) -> &mut Line {
        self.s.extend(std::iter::repeat_n(' ', n));
        self.w += n;
        self
    }

    fn to(&mut self, col: usize) -> &mut Line {
        let n = col.saturating_sub(self.w);
        self.pad(n)
    }

    fn put(&mut self, p: &Paint, c: Rgb, text: &str) -> &mut Line {
        p.fg(&mut self.s, c);
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
