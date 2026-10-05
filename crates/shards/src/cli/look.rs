//! How shards' own pages look on a colour terminal, in hyperlight's design: help for the
//! root, for a management command and for each command, and errors in a panel. The head
//! is pull's: the mark beside the brand and the page's name in the site's tracked
//! capitals. Off a terminal, or without colour, these pages are Docker's text instead.

use std::io::Write as _;

use shards_cmdline::catalog::{self, Entry};
use shards_cmdline::flags::{self, Command};
use shards_tui::canvas::Canvas;
use shards_tui::layout;
use shards_tui::text;
use shards_tui::tokens::{self, Paint, Rgb};

/// A line being made: its text, with colours, and its visible width.
#[derive(Default)]
pub(super) struct Line {
    pub(super) s: String,
    pub(super) w: usize,
}

impl Line {
    pub(super) fn pad(&mut self, n: usize) -> &mut Line {
        self.s.extend(std::iter::repeat_n(' ', n));
        self.w += n;
        self
    }

    pub(super) fn to(&mut self, col: usize) -> &mut Line {
        let n = col.saturating_sub(self.w);
        self.pad(n)
    }

    pub(super) fn put(&mut self, p: &Paint, c: Rgb, text: &str) -> &mut Line {
        p.fg(&mut self.s, c);
        self.s.push_str(text);
        self.w += text.chars().count();
        self
    }

    pub(super) fn bold(&mut self, p: &Paint, on: bool) -> &mut Line {
        p.bold(&mut self.s, on);
        self
    }
}

/// A page being made.
pub(super) struct Page {
    lines: Vec<Line>,
    spare: Line,
}

impl Page {
    pub(super) fn new() -> Page {
        Page {
            lines: Vec::new(),
            spare: Line::default(),
        }
    }

    pub(super) fn line(&mut self) -> &mut Line {
        let at = self.lines.len();
        self.lines.push(Line::default());
        // Just pushed, so there; the fallback is never taken.
        self.lines.get_mut(at).unwrap_or(&mut self.spare)
    }

    pub(super) fn blank(&mut self) {
        self.lines.push(Line::default());
    }

    pub(super) fn write(self, p: &Paint, out: &mut impl std::io::Write) {
        let mut text = String::new();
        for mut l in self.lines {
            p.reset(&mut l.s);
            text.push_str(&l.s);
            text.push('\n');
        }
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}

/// The name pages are titled with, where it is not their command's: the words a page
/// was asked for by (`list vms`), set once, before any page is drawn.
static NAMED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Titles this process's pages `name`.
pub(crate) fn name_as(name: &str) {
    let _ = NAMED.set(name.to_string());
}

/// An action's page: how it is said, and what it does to each thing it takes.
pub(crate) fn action(p: &Paint, action: &str, rows: &[(&str, &str)], out: &mut impl std::io::Write) {
    let cols = width();
    let mut page = Page::new();
    head(&mut page, p, cols, action, &[]);
    heading(&mut page, p, "usage");
    usage(&mut page, p, cols, &format!("shards {action} THING [ARG...]"));
    heading(&mut page, p, "things");
    let pad = rows.iter().map(|(t, _)| t.len()).max().unwrap_or(0) + 3;
    for (takes, about) in rows {
        let parts = layout::wrap(about, cols.saturating_sub(4 + pad).max(20));
        let l = page.line();
        l.pad(4).put(p, tokens::TEAL, takes);
        if let Some(first) = parts.first() {
            l.to(4 + pad).put(p, tokens::BODY, first);
        }
        for more in parts.iter().skip(1) {
            page.line().pad(4 + pad).put(p, tokens::BODY, more);
        }
    }
    page.write(p, out);
}

/// Whether stdout is a colour terminal, without asking it anything.
pub(crate) fn styled_quiet() -> bool {
    // SAFETY: isatty(3) on this process's stdout.
    let tty = unsafe { libc::isatty(1) } == 1;
    tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
}

/// The paint for stdout, if it is a colour terminal: shards' pages are drawn there.
pub(crate) fn styled() -> Option<Paint> {
    // SAFETY: isatty(3) on this process's stdout.
    let tty = unsafe { libc::isatty(1) } == 1;
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
    if !(tty && color) {
        return None;
    }
    let env = |k: &str| std::env::var(k).ok();
    let mut paint = Paint::new(shards_tui::tokens_truecolor(&env));
    if let Some(page) = super::terminal::background() {
        paint.page = page;
    }
    Some(paint)
}

/// The paint for stderr, if it is a colour terminal, for errors.
pub(crate) fn styled_err() -> Option<Paint> {
    // SAFETY: isatty(3) on this process's stderr.
    let tty = unsafe { libc::isatty(2) } == 1;
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
    let env = |k: &str| std::env::var(k).ok();
    (tty && color).then(|| Paint::new(shards_tui::tokens_truecolor(&env)))
}

/// The size of the terminal stderr is, rows then columns; 0×0 where it is none.
pub(crate) fn err_size() -> (usize, usize) {
    let (r, c) = super::terminal::size(2);
    (usize::from(r), usize::from(c))
}

/// The terminal's width, or 80.
pub(super) fn width() -> usize {
    match super::terminal::size(1).1 {
        0 => 80,
        w => usize::from(w),
    }
}

/// The head: the mark, where there is room, beside the brand and `name` in tracked
/// capitals, then `lines` under them.
pub(super) fn head(page: &mut Page, p: &Paint, cols: usize, name: &str, lines: &[(Rgb, bool, String)]) {
    let with_mark = cols >= 56;
    let mut mark = Canvas::new(8, 4);
    if with_mark {
        // A still of the mark: a page is drawn once.
        shards_tui::mark::draw(&mut mark, 1.7);
    }
    let room = cols.saturating_sub(if with_mark { 11 } else { 1 });
    let mut info: Vec<Line> = Vec::new();
    let mut brand = Line::default();
    let letters = text::eyebrow("shards");
    let n = letters.chars().count().max(2) - 1;
    brand.bold(p, true);
    for (k, ch) in letters.chars().enumerate() {
        brand.put(p, tokens::prism(k as f64 / n as f64), &ch.to_string());
    }
    brand.bold(p, false);
    let name = NAMED.get().map_or(name, String::as_str);
    let brow = text::eyebrow(name);
    if !name.is_empty() && brand.w + 5 + brow.chars().count() <= room {
        brand
            .put(p, tokens::SUBTLE, "  ·  ")
            .put(p, tokens::EYEBROW, &brow);
    }
    info.push(brand);
    for (c, bold, words) in lines {
        for (i, part) in layout::wrap(words, room).into_iter().enumerate() {
            let mut l = Line::default();
            l.bold(p, *bold && i == 0).put(p, *c, &part).bold(p, false);
            info.push(l);
        }
    }
    let rows = info.len().max(if with_mark { mark.rows() } else { 0 });
    let mut info = info.into_iter();
    for r in 0..rows {
        let l = page.line();
        if with_mark {
            if r < mark.rows() {
                mark.row(r, p, &mut l.s);
                l.w += 8;
            } else {
                l.pad(8);
            }
            l.pad(2);
        }
        if let Some(i) = info.next() {
            l.s.push_str(&i.s);
            l.w += i.w;
        }
    }
}

/// A section's heading: the site's eyebrow, in its grey.
pub(super) fn heading(page: &mut Page, p: &Paint, words: &str) {
    page.blank();
    // Plain capitals: tracked out, a heading over a list reads as letters, not a word.
    page.line().pad(2).put(p, tokens::EYEBROW, &words.to_uppercase());
}

/// A usage line, its words coloured by what they are: the command's own, its options,
/// and what it takes.
fn usage(page: &mut Page, p: &Paint, cols: usize, line: &str) {
    let l = page.line();
    l.pad(4);
    let mut first = true;
    for word in line.split(' ') {
        if !first {
            l.pad(1);
        }
        first = false;
        let c = if word.starts_with('[') && word.contains("OPTIONS") {
            tokens::SUBTLE
        } else if word.chars().any(|c| c.is_ascii_uppercase()) {
            tokens::TEAL
        } else {
            tokens::FOREGROUND
        };
        if l.w + word.chars().count() > cols {
            break;
        }
        l.put(p, c, word);
    }
}

/// Entries in columns: each name in the foreground, what it does in the body's grey.
fn entries(page: &mut Page, p: &Paint, cols: usize, entries: &[Entry]) {
    let pad = entries.iter().map(|e| e.name.len()).max().unwrap_or(0) + 3;
    let about_w = cols.saturating_sub(4 + pad);
    for e in entries {
        let parts = if about_w >= 20 {
            layout::wrap(e.about, about_w)
        } else {
            Vec::new()
        };
        let l = page.line();
        l.pad(4).put(p, tokens::FOREGROUND, e.name);
        if let Some(first) = parts.first() {
            l.to(4 + pad).put(p, tokens::BODY, first);
        }
        for more in parts.iter().skip(1) {
            page.line().pad(4 + pad).put(p, tokens::BODY, more);
        }
    }
}

/// The root's help: what shards is, and its commands by group.
pub fn top(p: &Paint, out: &mut impl std::io::Write) {
    let cols = width();
    let mut page = Page::new();
    head(
        &mut page,
        p,
        cols,
        "",
        &[(tokens::BRIGHT, true, catalog::ABOUT.to_string())],
    );
    heading(&mut page, p, "usage");
    usage(&mut page, p, cols, "shards ACTION THING [ARG...]");
    // The actions, a section for each thing they act on, each in order.
    let pad = shards_cmdline::grammar::ACTIONS
        .iter()
        .map(|(_, a, t, _)| a.len() + 1 + t.len())
        .max()
        .unwrap_or(0)
        + 3;
    for section in shards_cmdline::grammar::SECTIONS {
        heading(&mut page, p, section);
        for (_, action, takes, about) in shards_cmdline::grammar::ACTIONS
            .iter()
            .filter(|r| r.0 == *section)
        {
            let parts = layout::wrap(about, cols.saturating_sub(4 + pad).max(20));
            let l = page.line();
            l.pad(4)
                .put(p, tokens::FOREGROUND, action)
                .pad(1)
                .put(p, tokens::TEAL, takes);
            if let Some(first) = parts.first() {
                l.to(4 + pad).put(p, tokens::BODY, first);
            }
            for more in parts.iter().skip(1) {
                page.line().pad(4 + pad).put(p, tokens::BODY, more);
            }
        }
    }
    page.blank();
    let l = page.line();
    l.pad(2).put(p, tokens::TEAL, "shards ACTION");
    l.put(p, tokens::SUBTLE, " shows what an action takes; ");
    l.put(p, tokens::TEAL, "shards ACTION THING --help");
    l.put(p, tokens::SUBTLE, " its options.");
    page.write(p, out);
}

/// A management command's help: what it manages, and its commands.
pub fn management(p: &Paint, name: &str, out: &mut impl std::io::Write) -> bool {
    let Some((about, list)) = catalog::management(name) else {
        return false;
    };
    let cols = width();
    let mut page = Page::new();
    head(
        &mut page,
        p,
        cols,
        name,
        &[(tokens::BRIGHT, true, about.to_string())],
    );
    heading(&mut page, p, "usage");
    usage(&mut page, p, cols, &format!("shards {name} COMMAND"));
    heading(&mut page, p, "commands");
    entries(&mut page, p, cols, list);
    page.blank();
    let l = page.line();
    l.pad(2).put(p, tokens::SUBTLE, "Run ");
    l.put(p, tokens::TEAL, &format!("shards {name} COMMAND --help"));
    l.put(p, tokens::SUBTLE, " for more on a command.");
    page.write(p, out);
    true
}

/// A command's help: what it does, how it is used, what else names it, and its options,
/// Docker's and shards' own apart.
pub fn command(p: &Paint, command: &Command, path: &str, out: &mut impl std::io::Write) {
    let cols = width();
    let mut page = Page::new();
    let name = path.strip_prefix("shards ").unwrap_or(path);
    head(
        &mut page,
        p,
        cols,
        name,
        &[(tokens::BRIGHT, true, command.about.trim().to_string())],
    );
    heading(&mut page, p, "usage");
    usage(&mut page, p, cols, format!("{path} {}", command.usage).trim_end());
    if !command.aliases.is_empty() {
        heading(&mut page, p, "aliases");
        for alias in command.aliases.split(", ") {
            page.line().pad(4).put(p, tokens::FOREGROUND, alias);
        }
    }
    // Docker's options and shards' own in one set of columns.
    let names = |f: &flags::Shown| -> usize {
        4 + 2 + f.name.len() + if f.value.is_empty() { 0 } else { 1 + f.value.len() }
    };
    let sections = [("options", false), ("shards options", true)]
        .map(|(title, extension)| (title, flags::shown(command.flags, extension)));
    let pad = sections
        .iter()
        .flat_map(|(_, shown)| shown.iter().map(names))
        .max()
        .unwrap_or(0)
        + 3;
    for (title, shown) in &sections {
        if shown.is_empty() {
            continue;
        }
        heading(&mut page, p, title);
        options(&mut page, p, cols, shown, pad, *title == "shards options");
    }
    page.write(p, out);
}

/// Options in columns: the short form in lavender, the long in the foreground, its
/// value's kind subtle; what it does wrapped beside them, or under them where narrow;
/// its default after, faint.
fn options(page: &mut Page, p: &Paint, cols: usize, shown: &[flags::Shown], pad: usize, own: bool) {
    let beside = cols >= 4 + pad + 28;
    let about_w = if beside {
        cols.saturating_sub(4 + pad)
    } else {
        cols.saturating_sub(8)
    };
    for f in shown {
        let l = page.line();
        // Shards' own, marked in the margin as the site marks what is new; the columns
        // stay the others'.
        if own {
            l.pad(2).put(p, tokens::LAVENDER, "◆ ");
        } else {
            l.pad(4);
        }
        match f.short {
            Some(s) => {
                l.put(p, tokens::LAVENDER, &format!("-{s}"));
                l.put(p, tokens::SUBTLE, ", ");
            }
            None => {
                l.pad(4);
            }
        }
        l.put(p, tokens::FOREGROUND, &format!("--{}", f.name));
        if !f.value.is_empty() {
            l.pad(1).put(p, tokens::SUBTLE, &f.value);
        }
        let mut about = f.usage.clone();
        if let Some(d) = &f.default {
            about.push_str(&format!(" (default {d})"));
        }
        let parts = layout::wrap(&about, about_w.max(10));
        if beside {
            if let Some(first) = parts.first() {
                l.to(4 + pad);
                put_about(l, p, first);
            }
            for more in parts.iter().skip(1) {
                let l = page.line();
                l.pad(4 + pad);
                put_about(l, p, more);
            }
        } else {
            for part in &parts {
                let l = page.line();
                l.pad(8);
                put_about(l, p, part);
            }
        }
    }
}

/// What an option does, its default in a fainter grey.
fn put_about(l: &mut Line, p: &Paint, part: &str) {
    match part.find("(default ") {
        Some(at) => {
            let (body, rest) = part.split_at(at);
            // The default, to its closing parenthesis; what follows it is the body again.
            let end = rest.find(')').map_or(rest.len(), |e| e + 1);
            let (default, after) = rest.split_at(end);
            l.put(p, tokens::BODY, body)
                .put(p, tokens::FAINT, default)
                .put(p, tokens::BODY, after);
        }
        None => {
            l.put(p, tokens::BODY, part);
        }
    }
}

/// `shards version`: what this shards is, where it runs, and what it runs its microVMs
/// with.
pub(crate) fn version(p: &Paint, out: &mut impl std::io::Write) {
    let cols = width();
    let mut page = Page::new();
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let os = std::env::consts::OS;
    head(
        &mut page,
        p,
        cols,
        "version",
        &[(
            tokens::BRIGHT,
            true,
            format!("shards {}", env!("CARGO_PKG_VERSION")),
        )],
    );
    page.blank();
    let backend = if cfg!(target_os = "macos") {
        "Hypervisor.framework"
    } else if cfg!(target_os = "linux") {
        "KVM"
    } else if cfg!(windows) {
        "Windows Hypervisor Platform"
    } else {
        "none"
    };
    let home = shards_ipc::home().ok();
    let kernel = crate::kernel::KERNEL.map(|k| {
        let release = k.name.split('-').nth(1).unwrap_or(k.name);
        let stored = home
            .as_ref()
            .is_some_and(|h| h.join("guest").join(format!("sha256-{}", k.sha256)).is_file());
        format!(
            "Linux {release} · {}",
            if stored {
                "stored"
            } else {
                "fetched on the first run"
            }
        )
    });
    let binary = std::env::current_exe().ok().map(|b| {
        let size = std::fs::metadata(&b).map(|m| m.len()).unwrap_or(0);
        format!("{} · {}", b.display(), text::bytes(size))
    });
    let rows: [(&str, Option<String>, Rgb); 6] = [
        ("host", Some(format!("{os}/{arch}")), tokens::FOREGROUND),
        ("guests", Some(format!("linux/{arch}")), tokens::FOREGROUND),
        ("hypervisor", Some(backend.to_string()), tokens::LAVENDER),
        ("kernel", kernel, tokens::FOREGROUND),
        ("home", home.map(|h| h.display().to_string()), tokens::MUTED),
        ("binary", binary, tokens::MUTED),
    ];
    for (label, value, c) in rows {
        if let Some(value) = value {
            let l = page.line();
            l.pad(4).put(p, tokens::EYEBROW, &format!("{label:<12}"));
            l.put(p, c, &layout::clip(&value, cols.saturating_sub(16)));
        }
    }
    page.write(p, out);
}

/// `shards run -d`: the microVM started, its ID, image and name, and what to do next.
pub(crate) fn started(p: &Paint, id: &str, image: &str, name: Option<&str>, out: &mut impl std::io::Write) {
    let cols = width();
    let short = id.get(..12).unwrap_or(id);
    let mut page = Page::new();
    head(
        &mut page,
        p,
        cols,
        "run",
        &[(tokens::MUTED, false, "running in the background".to_string())],
    );
    page.blank();
    let l = page.line();
    l.pad(4).put(p, tokens::SAGE, "● ").bold(p, true);
    l.put(p, tokens::BRIGHT, name.unwrap_or(short)).bold(p, false);
    l.pad(2)
        .put(p, tokens::MUTED, "from ")
        .put(p, tokens::FOREGROUND, image);
    page.blank();
    let rows = [("id", id.to_string(), tokens::FOREGROUND)];
    for (label, value, c) in rows {
        let l = page.line();
        l.pad(4).put(p, tokens::EYEBROW, &format!("{label:<10}"));
        l.put(p, c, &layout::clip(&value, cols.saturating_sub(15)));
    }
    let handle = name.unwrap_or(short);
    for (label, command) in [
        ("follow", format!("shards logs -f {handle}")),
        ("stop", format!("shards stop {handle}")),
    ] {
        let l = page.line();
        l.pad(4).put(p, tokens::EYEBROW, &format!("{label:<10}"));
        l.put(p, tokens::TEAL, &command);
    }
    page.write(p, out);
}

/// The help of one of shards' own commands, from its usage text: `usage:` lines, then
/// what it does, then `word: what it means` entries, `SHARDS_…` ones its environment;
/// each entry's further lines indented under it.
pub(crate) fn usage_page(name: &str, text: &str) -> bool {
    let Some(p) = styled() else {
        return false;
    };
    let cols = width();
    let mut uses: Vec<String> = Vec::new();
    let mut about: Vec<String> = Vec::new();
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(u) = line.strip_prefix("usage: ") {
            uses.push(u.trim().to_string());
        } else if trimmed.starts_with("shards ") && about.is_empty() && entries.is_empty() {
            uses.push(trimmed.to_string());
        } else if line.starts_with("    ") {
            // More of the last entry.
            if let Some(last) = env.last_mut().or(entries.last_mut()) {
                last.1.push(' ');
                last.1.push_str(trimmed);
            } else {
                about.push(trimmed.to_string());
            }
        } else if let Some((key, rest)) = trimmed.split_once(": ").filter(|(k, _)| !k.contains(' ')) {
            let entry = (key.to_string(), rest.to_string());
            if key.starts_with("SHARDS_") {
                env.push(entry);
            } else {
                entries.push(entry);
            }
        } else if !trimmed.is_empty() {
            about.push(trimmed.to_string());
        }
    }
    let mut page = Page::new();
    head(
        &mut page,
        &p,
        cols,
        name,
        &[(tokens::BRIGHT, true, about.join(" "))],
    );
    heading(&mut page, &p, "usage");
    for u in &uses {
        usage(&mut page, &p, cols, u);
    }
    let pad = entries
        .iter()
        .chain(&env)
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0)
        + 3;
    for (title, list) in [("options", &entries), ("environment", &env)] {
        if list.is_empty() {
            continue;
        }
        heading(&mut page, &p, title);
        for (key, what) in list {
            let parts = layout::wrap(what, cols.saturating_sub(4 + pad).max(20));
            let l = page.line();
            l.pad(4).put(&p, tokens::FOREGROUND, key);
            if let Some(first) = parts.first() {
                l.to(4 + pad);
                put_about(l, &p, first);
            }
            for more in parts.iter().skip(1) {
                let l = page.line();
                l.pad(4 + pad);
                put_about(l, &p, more);
            }
        }
    }
    page.write(&p, &mut std::io::stdout().lock());
    true
}

/// `text` in shards' words: what Docker's text calls a container is a microVM here.
pub(crate) fn ours(text: &str) -> String {
    text.replace("containers", "microVMs")
        .replace("Containers", "MicroVMs")
        .replace("container", "microVM")
        .replace("Container", "MicroVM")
}

/// What the daemon said went wrong, in a panel on stderr; under the head, unless a page
/// was drawn above it.
pub(crate) fn panel(p: &Paint, message: &str, with_head: bool) {
    let message = &ours(message);
    if with_head {
        error(p, "shards", message, &[]);
        return;
    }
    let cols = match super::terminal::size(2).1 {
        0 => 80,
        w => usize::from(w),
    };
    let mut text = String::from("\n");
    for (s, _) in shards_tui::panel::error(p, cols, "", message, &[]) {
        text.push_str(&s);
        text.push_str("\x1b[0m\n");
    }
    let _ = std::io::stderr().write_all(text.as_bytes());
}

/// An error, in a panel on stderr: `title` the command it came from, `message` what
/// happened, and `hints` what to do.
pub fn error(p: &Paint, title: &str, message: &str, hints: &[&str]) {
    let cols = match super::terminal::size(2).1 {
        0 => 80,
        w => usize::from(w),
    };
    // The head, as every page has it, naming the command the error came from.
    let mut page = Page::new();
    head(
        &mut page,
        p,
        cols,
        if title == "shards" { "" } else { title },
        &[],
    );
    page.blank();
    page.write(p, &mut std::io::stderr().lock());
    let lines = shards_tui::panel::error(p, cols, "", message, hints);
    let mut text = String::new();
    for (s, _) in lines {
        text.push_str(&s);
        text.push_str("\x1b[0m\n");
    }
    let _ = std::io::stderr().write_all(text.as_bytes());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn seen(s: &[u8]) -> String {
        let s = String::from_utf8_lossy(s);
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

    #[test]
    fn every_command_reads_on_its_page() {
        let p = Paint::new(true);
        let mut out = Vec::new();
        top(&p, &mut out);
        let text = seen(&out);
        for (_, action, takes, _) in shards_cmdline::grammar::ACTIONS {
            assert!(text.contains(&format!("{action} {takes}")), "{action} {takes}");
        }
        assert!(text.contains("S H A R D S"));
        let mut out = Vec::new();
        command(&p, &shards_cmdline::commands::PULL, "shards pull", &mut out);
        let text = seen(&out);
        for f in [
            "--all-tags",
            "--platform",
            "--quiet",
            "--no-cache",
            "--output-agentfile",
        ] {
            assert!(text.contains(f), "{f}: {text}");
        }
        assert!(text.contains("P U L L"));
        assert!(text.contains("SHARDS OPTIONS"));
        let mut out = Vec::new();
        assert!(management(&p, "image", &mut out));
        assert!(seen(&out).contains("ls"));
        assert!(!management(&p, "network", &mut Vec::new()));
    }
}
