//! The pages commands the daemon runs draw on a colour terminal, from the records it
//! sends ([`shards_ipc::Sheet`]): each its own, in pull's look. A sheet this client has
//! no page for is shown as its records, plainly.

use shards_ipc::Sheet;
use shards_tui::bar::{self, Fill};
use shards_tui::layout;
use shards_tui::text;
use shards_tui::tokens::{self, Paint};

use super::look::{self, Page};

/// Draws `sheet`'s page on stdout.
pub fn show(sheet: &Sheet) {
    let Some(p) = look::styled() else {
        return;
    };
    let mut page = Page::new();
    match sheet.name.as_str() {
        "images" => images(&mut page, &p, sheet),
        "ps" => ps(&mut page, &p, sheet),
        "ended" => ended(&mut page, &p, sheet),
        "rmi" => rmi(&mut page, &p, sheet),
        "tag" => tag(&mut page, &p, sheet),
        "port" => port(&mut page, &p, sheet),
        "history" => history(&mut page, &p, sheet),
        "df" => disk(&mut page, &p, sheet),
        "prune" => prune(&mut page, &p, sheet),
        "json" => {
            json(&p, sheet.get(0, "text").unwrap_or(""));
            return;
        }
        _ => plain(&mut page, &p, sheet),
    }
    page.write(&p, &mut std::io::stdout().lock());
}

/// A sheet without a page of its own: each record a line of its fields.
fn plain(page: &mut Page, p: &Paint, sheet: &Sheet) {
    for record in &sheet.records {
        let l = page.line();
        l.pad(2);
        for (k, v) in record {
            l.put(p, tokens::EYEBROW, k)
                .pad(1)
                .put(p, tokens::FOREGROUND, v)
                .pad(2);
        }
    }
}

/// How long ago `unix` seconds was, in words.
fn ago(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    if unix <= 0 {
        return "—".into();
    }
    let s = now.saturating_sub(unix).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        86_400..172_800 => "yesterday".into(),
        _ if s < 60 * 86_400 => format!("{} days ago", s / 86_400),
        _ if s < 730 * 86_400 => format!("{} months ago", s / (30 * 86_400)),
        _ => format!("{} years ago", s / (365 * 86_400)),
    }
}

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

/// A cell of a table: its text, its colour, and whether it is bold.
struct Cell {
    text: String,
    color: tokens::Rgb,
    bold: bool,
    /// Drawn after the text, in the cell's room: a bar of so many cells, and its share.
    bar: Option<(usize, f64)>,
}

impl Cell {
    fn new(text: impl Into<String>, color: tokens::Rgb) -> Cell {
        Cell {
            text: text.into(),
            color,
            bold: false,
            bar: None,
        }
    }

    fn bold(mut self) -> Cell {
        self.bold = true;
        self
    }

    fn width(&self) -> usize {
        self.text.chars().count() + self.bar.map_or(0, |(w, _)| 1 + w)
    }
}

/// A column: its heading, whether its cells stand to the right, and how much it
/// matters when the terminal is narrow (higher stays longer).
struct Column {
    heading: &'static str,
    right: bool,
    keep: u8,
}

/// `rows` in `columns`, each as wide as its widest cell or heading, two cells apart,
/// headings in the eyebrow's capitals over their cells; a marker cell, two wide, before
/// each row. Columns that matter least give way first until the table fits `cols`.
fn table(page: &mut Page, p: &Paint, cols: usize, columns: &[Column], rows: &[(Cell, Vec<Cell>)]) {
    let mut widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            rows.iter()
                .filter_map(|(_, cells)| cells.get(c).map(Cell::width))
                .max()
                .unwrap_or(0)
                .max(col.heading.len())
        })
        .collect();
    let mut shown: Vec<bool> = vec![true; columns.len()];
    let used = |shown: &[bool], widths: &[usize]| -> usize {
        4 + 2
            + shown
                .iter()
                .zip(widths)
                .filter(|(s, _)| **s)
                .map(|(_, w)| w + 2)
                .sum::<usize>()
    };
    while used(&shown, &widths) > cols {
        // The least kept column still shown goes; the first, the name, never does.
        let drop = (1..columns.len())
            .filter(|&c| shown.get(c).copied().unwrap_or(false))
            .min_by_key(|&c| (columns.get(c).map_or(0, |col| col.keep), std::cmp::Reverse(c)));
        match drop {
            Some(c) => {
                if let Some(s) = shown.get_mut(c) {
                    *s = false;
                }
            }
            None => {
                // Only the first is left: it narrows to what there is.
                if let Some(w) = widths.first_mut() {
                    *w = cols.saturating_sub(4 + 2 + 2).max(4);
                }
                break;
            }
        }
    }
    let cell_out = |l: &mut look::Line, cell: &Cell, w: usize, right: bool| {
        let text = layout::clip(&cell.text, w.saturating_sub(cell.bar.map_or(0, |(b, _)| b + 1)));
        let n = text.chars().count() + cell.bar.map_or(0, |(b, _)| b + 1);
        if right {
            l.pad(w.saturating_sub(n));
        }
        l.bold(p, cell.bold).put(p, cell.color, &text).bold(p, false);
        if let Some((bw, share)) = cell.bar {
            l.pad(1);
            // Nothing, no bar; anything, at least a cell of one.
            let filled = if share > 0.0 {
                ((share.clamp(0.0, 1.0) * bw as f64).round() as usize).clamp(1, bw)
            } else {
                0
            };
            if filled > 0 {
                bar::draw(&mut l.s, p, filled, Fill::Done, 0.0, 0.0);
                l.w += filled;
            }
            l.pad(bw - filled);
        }
        if !right {
            l.pad(w.saturating_sub(n));
        }
        l.pad(2);
    };
    {
        let l = page.line();
        l.pad(6);
        for (c, col) in columns.iter().enumerate() {
            if !shown.get(c).copied().unwrap_or(false) {
                continue;
            }
            let w = widths.get(c).copied().unwrap_or(0);
            let heading = Cell::new(col.heading, tokens::EYEBROW);
            cell_out(l, &heading, w, col.right);
        }
    }
    for (marker, cells) in rows {
        let l = page.line();
        l.pad(4)
            .put(p, marker.color, &marker.text)
            .pad(2usize.saturating_sub(marker.text.chars().count()));
        for (c, cell) in cells.iter().enumerate() {
            if !shown.get(c).copied().unwrap_or(false) {
                continue;
            }
            let w = widths.get(c).copied().unwrap_or(0);
            cell_out(l, cell, w, columns.get(c).is_some_and(|col| col.right));
        }
    }
}

/// `shards images`: the store's totals, then each image a row: what it is (a microVM, or
/// an image stored for another platform), its name, ID and age, its layers, its size
/// (a microVM's disk, else its layers) drawn against the largest, its platform, and the
/// microVMs it runs as.
fn images(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let head = (0..sheet.records.len()).find(|&i| sheet.get(i, "kind") == Some("head"));
    let num = |i: usize, k: &str| sheet.get(i, k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let mut lines = Vec::new();
    if let Some(h) = head {
        let n = num(h, "images");
        lines.push((
            tokens::MUTED,
            false,
            format!(
                "{n} {} · {} of layers · {} of microVM disks",
                if n == 1 { "image" } else { "images" },
                text::bytes(num(h, "content")),
                text::bytes(num(h, "disk"))
            ),
        ));
        if let Some(store) = sheet.get(h, "store") {
            let free = match num(h, "free") {
                0 => String::new(),
                f => format!(" · {} free", text::bytes(f)),
            };
            lines.push((tokens::SUBTLE, false, format!("{}{free}", home_relative(store))));
        }
    }
    look::head(page, p, cols, "images", &lines);
    let rows: Vec<usize> = (0..sheet.records.len())
        .filter(|&i| sheet.get(i, "kind").is_none())
        .collect();
    page.blank();
    if rows.is_empty() {
        let l = page.line();
        l.pad(4).put(p, tokens::MUTED, "No images yet. ");
        l.put(p, tokens::TEAL, "shards pull image NAME");
        l.put(p, tokens::MUTED, " brings one, and makes it a microVM.");
        return;
    }
    let size = |i: usize| match num(i, "disk") {
        0 => num(i, "content"),
        d => d,
    };
    let largest = rows.iter().map(|&i| size(i)).max().unwrap_or(0).max(1);
    // Every size as wide as the widest, so the bars after them start together.
    let size_w = rows
        .iter()
        .map(|&i| text::bytes(size(i)).len())
        .max()
        .unwrap_or(0);
    let columns = [
        Column {
            heading: "NAME",
            right: false,
            keep: 9,
        },
        Column {
            heading: "TYPE",
            right: false,
            keep: 8,
        },
        Column {
            heading: "ID",
            right: false,
            keep: 5,
        },
        Column {
            heading: "CREATED",
            right: false,
            keep: 3,
        },
        Column {
            heading: "LAYERS",
            right: false,
            keep: 2,
        },
        Column {
            heading: "SIZE",
            right: false,
            keep: 7,
        },
        Column {
            heading: "PLATFORM",
            right: false,
            keep: 1,
        },
        Column {
            heading: "RUNNING",
            right: false,
            keep: 6,
        },
    ];
    let table_rows: Vec<(Cell, Vec<Cell>)> = rows
        .iter()
        .map(|&i| {
            let vm = num(i, "disk") > 0;
            let name = {
                let repo = sheet.get(i, "repo").unwrap_or("<none>");
                match sheet.get(i, "tag").filter(|t| *t != "<none>") {
                    Some(tag) => format!("{repo}:{tag}"),
                    None => repo.to_string(),
                }
            };
            let (running, stopped) = (num(i, "running"), num(i, "stopped"));
            let marker = if vm {
                Cell::new("●", tokens::SAGE)
            } else {
                Cell::new("●", tokens::FAINT)
            };
            let mut size_cell = Cell::new(
                format!("{:<size_w$}", text::bytes(size(i))),
                if vm { tokens::LAVENDER } else { tokens::MUTED },
            );
            size_cell.bar = Some((8, size(i) as f64 / largest as f64));
            let cells = vec![
                Cell::new(name, tokens::BRIGHT).bold(),
                Cell::new(
                    if vm { "microVM" } else { "image" },
                    if vm { tokens::SAGE } else { tokens::MUTED },
                ),
                Cell::new(sheet.get(i, "id").unwrap_or(""), tokens::SUBTLE),
                Cell::new(
                    ago(sheet.get(i, "created").and_then(|c| c.parse().ok()).unwrap_or(0)),
                    tokens::MUTED,
                ),
                Cell::new(num(i, "layers").to_string(), tokens::FOREGROUND),
                size_cell,
                Cell::new(sheet.get(i, "platform").unwrap_or(""), tokens::MUTED),
                match (running, stopped) {
                    (0, 0) => Cell::new("—", tokens::FAINT),
                    (0, s) => Cell::new(format!("{s} stopped"), tokens::MUTED),
                    (r, 0) => Cell::new(format!("● {r} running"), tokens::AMBER),
                    (r, s) => Cell::new(format!("● {r} running · {s} stopped"), tokens::AMBER),
                },
            ];
            (marker, cells)
        })
        .collect();
    table(page, p, cols, &columns, &table_rows);
}

/// `shards ps`: how many run, then each microVM a row: its name, ID, image, how it
/// stands, its ports and what it runs.
fn ps(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let head = (0..sheet.records.len()).find(|&i| sheet.get(i, "kind") == Some("head"));
    let num = |i: usize, k: &str| sheet.get(i, k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let mut lines = Vec::new();
    let all = head.and_then(|h| sheet.get(h, "all")) == Some("true");
    if let Some(h) = head {
        let (running, total) = (num(h, "running"), num(h, "total"));
        let stopped = total.saturating_sub(running);
        let mut words = format!("{running} running");
        if stopped > 0 {
            words.push_str(&format!(" · {stopped} stopped"));
            if !all {
                words.push_str(" (shards list vm -a lists them)");
            }
        }
        lines.push((tokens::MUTED, false, words));
    }
    look::head(page, p, cols, "ps", &lines);
    let rows: Vec<usize> = (0..sheet.records.len())
        .filter(|&i| sheet.get(i, "kind").is_none())
        .collect();
    page.blank();
    if rows.is_empty() {
        let l = page.line();
        l.pad(4).put(p, tokens::MUTED, "Nothing running. ");
        l.put(p, tokens::TEAL, "shards run vm IMAGE");
        l.put(p, tokens::MUTED, " starts a microVM from an image.");
        return;
    }
    let columns = [
        Column {
            heading: "MICROVM",
            right: false,
            keep: 9,
        },
        Column {
            heading: "ID",
            right: false,
            keep: 4,
        },
        Column {
            heading: "IMAGE",
            right: false,
            keep: 8,
        },
        Column {
            heading: "STATUS",
            right: false,
            keep: 7,
        },
        Column {
            heading: "PORTS",
            right: false,
            keep: 5,
        },
        Column {
            heading: "RUNS",
            right: false,
            keep: 2,
        },
    ];
    let table_rows: Vec<(Cell, Vec<Cell>)> = rows
        .iter()
        .map(|&i| {
            let state = sheet.get(i, "state").unwrap_or("");
            let failed = state == "exited" && sheet.get(i, "exit").is_some_and(|e| e != "0" && !e.is_empty());
            // Running, sage; created, lavender; ended well, grey; ended badly, rose.
            let (glyph, c) = match state {
                "running" => ("●", tokens::SAGE),
                "created" => ("●", tokens::LAVENDER),
                _ if failed => ("●", tokens::ROSE),
                _ => ("●", tokens::FAINT),
            };
            let status_c = match state {
                "running" => tokens::SAGE,
                _ if failed => tokens::ROSE,
                _ => tokens::MUTED,
            };
            let command = sheet.get(i, "command").unwrap_or("").trim_matches('"');
            let cells = vec![
                Cell::new(sheet.get(i, "name").unwrap_or(""), tokens::BRIGHT).bold(),
                Cell::new(sheet.get(i, "id").unwrap_or(""), tokens::SUBTLE),
                Cell::new(sheet.get(i, "image").unwrap_or(""), tokens::FOREGROUND),
                Cell::new(look::ours(sheet.get(i, "status").unwrap_or("")), status_c),
                Cell::new(sheet.get(i, "ports").unwrap_or(""), tokens::TEAL),
                Cell::new(layout::clip(command, 40), tokens::MUTED),
            ];
            (Cell::new(glyph, c), cells)
        })
        .collect();
    table(page, p, cols, &columns, &table_rows);
}

/// `shards stop`, `kill` and `rm`: what became of each microVM named, how it ended and
/// in how long, all ended at once.
fn ended(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let head = (0..sheet.records.len()).find(|&i| sheet.get(i, "kind") == Some("head"));
    let verb = head.and_then(|h| sheet.get(h, "verb")).unwrap_or("stop");
    let done = match verb {
        "rm" => "removed",
        "kill" => "killed",
        _ => "stopped",
    };
    let rows: Vec<usize> = (0..sheet.records.len())
        .filter(|&i| sheet.get(i, "kind").is_none())
        .collect();
    let ok = rows
        .iter()
        .filter(|&&i| sheet.get(i, "outcome") == Some("ok"))
        .count();
    let failed = rows
        .iter()
        .filter(|&&i| sheet.get(i, "outcome") == Some("error"))
        .count();
    let longest = rows
        .iter()
        .filter_map(|&i| sheet.get(i, "ms").and_then(|m| m.parse::<u64>().ok()))
        .max()
        .unwrap_or(0);
    let mut summary = format!("{ok} {done}");
    if longest > 0 {
        summary.push_str(&format!(
            ", all at once, in {}",
            text::duration(longest as f64 / 1000.0)
        ));
    }
    if failed > 0 {
        summary.push_str(&format!(" · {failed} could not be"));
    }
    look::head(page, p, cols, verb, &[(tokens::MUTED, false, summary)]);
    page.blank();
    let name_w = rows
        .iter()
        .map(|&i| sheet.get(i, "target").unwrap_or("").chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 32);
    for &i in &rows {
        let target = layout::clip(sheet.get(i, "target").unwrap_or(""), name_w);
        let n = target.chars().count();
        let l = page.line();
        l.pad(4);
        match sheet.get(i, "outcome") {
            Some("error") => {
                l.put(p, tokens::ROSE, "○ ")
                    .bold(p, true)
                    .put(p, tokens::BRIGHT, &target)
                    .bold(p, false);
                l.pad(name_w - n + 2);
                let room = cols.saturating_sub(l.w);
                l.put(
                    p,
                    tokens::ROSE,
                    &layout::clip(sheet.get(i, "error").unwrap_or(""), room),
                );
            }
            Some("none") => {
                l.put(p, tokens::SUBTLE, "· ").put(p, tokens::MUTED, &target);
                l.pad(name_w - n + 2).put(p, tokens::SUBTLE, "not there");
            }
            _ => {
                l.put(p, tokens::SAGE, "● ")
                    .bold(p, true)
                    .put(p, tokens::BRIGHT, &target)
                    .bold(p, false);
                l.pad(name_w - n + 2).put(p, tokens::SAGE, done);
                let how = match sheet.get(i, "how") {
                    Some("signal") => "on its stop signal",
                    Some("kill") => "by SIGKILL",
                    Some("escalated") => "by SIGKILL once its grace ran out",
                    Some("vm") => "its microVM killed",
                    Some("already") => "had already ended",
                    _ => "",
                };
                if !how.is_empty() {
                    l.pad(2).put(p, tokens::MUTED, how);
                }
                if let Some(ms) = sheet
                    .get(i, "ms")
                    .and_then(|m| m.parse::<u64>().ok())
                    .filter(|m| *m > 0)
                {
                    l.pad(2)
                        .put(p, tokens::SUBTLE, &text::duration(ms as f64 / 1000.0));
                }
            }
        }
    }
}

/// `shards rmi`: each image removed: the names it lost, whether its data went or is kept
/// for a microVM still made from it, the stopped microVMs removed with it, each refusal.
fn rmi(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let n = sheet.records.len();
    // Each image asked for, in order, with what became of it.
    let mut asked: Vec<&str> = Vec::new();
    for i in 0..n {
        if let Some(g) = sheet.get(i, "given")
            && !asked.contains(&g)
        {
            asked.push(g);
        }
    }
    let of = |g: &str, k: &str| -> Vec<&str> {
        (0..n)
            .filter(|&i| sheet.get(i, "given") == Some(g))
            .filter_map(|i| sheet.get(i, k))
            .collect()
    };
    let removed = asked.iter().filter(|g| of(g, "error").is_empty()).count();
    let refused = asked.len() - removed;
    let freed: u64 = (0..n)
        .filter_map(|i| sheet.get(i, "freed").and_then(|f| f.parse::<u64>().ok()))
        .sum();
    let mut summary = format!(
        "{removed} {} removed",
        if removed == 1 { "image" } else { "images" }
    );
    if freed > 0 {
        summary.push_str(&format!(" · {} freed", text::bytes(freed)));
    }
    if refused > 0 {
        summary.push_str(&format!(" · {refused} refused"));
    }
    look::head(page, p, cols, "rmi", &[(tokens::MUTED, false, summary)]);
    page.blank();
    for g in &asked {
        let l = page.line();
        l.pad(4);
        if let Some(e) = of(g, "error").first().copied() {
            l.put(p, tokens::ROSE, "● ")
                .bold(p, true)
                .put(p, tokens::BRIGHT, g)
                .bold(p, false);
            let room = cols.saturating_sub(l.w + 2);
            l.pad(2).put(p, tokens::ROSE, &layout::clip(&look::ours(e), room));
            continue;
        }
        l.put(p, tokens::SAGE, "● ")
            .bold(p, true)
            .put(p, tokens::BRIGHT, g)
            .bold(p, false);
        let deleted = of(g, "deleted").first().copied();
        match deleted {
            Some(id) => {
                l.pad(2).put(p, tokens::SAGE, "removed");
                let f = of(g, "freed")
                    .first()
                    .and_then(|f| f.parse::<u64>().ok())
                    .unwrap_or(0);
                let what = if f > 0 {
                    format!("  {id} · {} freed with its microVM disk", text::bytes(f))
                } else {
                    format!("  {id}")
                };
                l.put(p, tokens::MUTED, &what);
            }
            None => {
                l.pad(2).put(p, tokens::SAGE, "removed");
                l.put(
                    p,
                    tokens::AMBER,
                    "  its disk is kept while a microVM made from it remains",
                );
            }
        }
        for name in of(g, "untagged") {
            let l = page.line();
            l.pad(8)
                .put(p, tokens::SUBTLE, "untagged ")
                .put(p, tokens::FOREGROUND, name);
        }
        for name in of(g, "removed_vm") {
            let l = page.line();
            l.pad(8)
                .put(p, tokens::SUBTLE, "stopped microVM removed ")
                .put(p, tokens::FOREGROUND, name);
        }
    }
}

/// `shards history`: how an image's layers were made, newest first: when, by what, and
/// each layer's size drawn against the largest; the steps that made no layer quieter.
fn history(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let image = sheet.get(0, "image").unwrap_or("");
    let id = sheet.get(0, "id").unwrap_or("");
    let rows: Vec<usize> = (1..sheet.records.len()).collect();
    let num = |i: usize, k: &str| sheet.get(i, k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let layers = rows
        .iter()
        .filter(|&&i| sheet.get(i, "empty") != Some("true"))
        .count();
    let total: i64 = rows.iter().map(|&i| num(i, "size")).sum();
    look::head(
        page,
        p,
        cols,
        "history",
        &[
            (tokens::BRIGHT, true, image.to_string()),
            (
                tokens::MUTED,
                false,
                format!(
                    "{id} · {} steps · {layers} layers · {} compressed",
                    rows.len(),
                    text::bytes(u64::try_from(total).unwrap_or(0))
                ),
            ),
        ],
    );
    page.blank();
    let largest = rows.iter().map(|&i| num(i, "size")).max().unwrap_or(0).max(1);
    let size_w = rows
        .iter()
        .map(|&i| text::bytes(u64::try_from(num(i, "size")).unwrap_or(0)).len())
        .max()
        .unwrap_or(0);
    let columns = [
        Column {
            heading: "CREATED",
            right: false,
            keep: 5,
        },
        Column {
            heading: "SIZE",
            right: false,
            keep: 7,
        },
        Column {
            heading: "CREATED BY",
            right: false,
            keep: 9,
        },
    ];
    let table_rows: Vec<(Cell, Vec<Cell>)> = rows
        .iter()
        .map(|&i| {
            let empty = sheet.get(i, "empty") == Some("true");
            let size = u64::try_from(num(i, "size")).unwrap_or(0);
            let mut size_cell = Cell::new(
                format!(
                    "{:<size_w$}",
                    if empty {
                        "—".to_string()
                    } else {
                        text::bytes(size)
                    }
                ),
                if empty { tokens::FAINT } else { tokens::LAVENDER },
            );
            if !empty {
                size_cell.bar = Some((8, size as f64 / largest as f64));
            }
            let by = sheet.get(i, "by").unwrap_or("").replace('\t', " ");
            // A RUN said as it was written, without the shell it ran in.
            let by = match by.strip_prefix("RUN /bin/sh -c ") {
                Some(rest) => format!("RUN {rest}"),
                None => by,
            };
            let marker = if empty {
                Cell::new("●", tokens::FAINT)
            } else {
                Cell::new("●", tokens::SAGE)
            };
            (
                marker,
                vec![
                    Cell::new(ago(num(i, "created")), tokens::MUTED),
                    size_cell,
                    Cell::new(
                        layout::clip(&by, 80),
                        if empty { tokens::MUTED } else { tokens::FOREGROUND },
                    ),
                ],
            )
        })
        .collect();
    table(page, p, cols, &columns, &table_rows);
}

/// `shards inspect disk`: what images, microVMs and their templates take, each drawn
/// against the largest, with what removing the unused would free.
fn disk(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let num = |i: usize, k: &str| sheet.get(i, k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let n = sheet.records.len();
    let all: u64 = (0..n).map(|i| num(i, "size")).sum();
    let freeable: u64 = (0..n).map(|i| num(i, "reclaimable")).sum();
    look::head(
        page,
        p,
        cols,
        "disk",
        &[(
            tokens::MUTED,
            false,
            format!(
                "{} in all · {} could be freed",
                text::bytes(all),
                text::bytes(freeable)
            ),
        )],
    );
    page.blank();
    let largest = (0..n).map(|i| num(i, "size")).max().unwrap_or(0).max(1);
    let size_w = (0..n)
        .map(|i| text::bytes(num(i, "size")).len())
        .max()
        .unwrap_or(0);
    let columns = [
        Column {
            heading: "WHAT",
            right: false,
            keep: 9,
        },
        Column {
            heading: "COUNT",
            right: false,
            keep: 6,
        },
        Column {
            heading: "IN USE",
            right: false,
            keep: 5,
        },
        Column {
            heading: "SIZE",
            right: false,
            keep: 8,
        },
        Column {
            heading: "FREEABLE",
            right: false,
            keep: 7,
        },
    ];
    let rows: Vec<(Cell, Vec<Cell>)> = (0..n)
        .map(|i| {
            let mut size = Cell::new(
                format!("{:<size_w$}", text::bytes(num(i, "size"))),
                tokens::LAVENDER,
            );
            size.bar = Some((12, num(i, "size") as f64 / largest as f64));
            let free = num(i, "reclaimable");
            (
                Cell::new("●", if free > 0 { tokens::AMBER } else { tokens::SAGE }),
                vec![
                    Cell::new(sheet.get(i, "kind").unwrap_or(""), tokens::BRIGHT).bold(),
                    Cell::new(num(i, "total").to_string(), tokens::FOREGROUND),
                    Cell::new(num(i, "active").to_string(), tokens::FOREGROUND),
                    size,
                    Cell::new(
                        if free > 0 { text::bytes(free) } else { "—".into() },
                        if free > 0 { tokens::AMBER } else { tokens::FAINT },
                    ),
                ],
            )
        })
        .collect();
    table(page, p, cols, &columns, &rows);
}

/// `shards prune ...`: the microVMs and images removed, and what that freed.
fn prune(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let n = sheet.records.len();
    let freed = sheet
        .get(0, "reclaimed")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let vms = (0..n).filter(|&i| sheet.get(i, "vm").is_some()).count();
    let images = (0..n).filter(|&i| sheet.get(i, "deleted").is_some()).count();
    look::head(
        page,
        p,
        cols,
        "prune",
        &[(
            tokens::MUTED,
            false,
            format!(
                "{vms} {} · {images} {} · {} freed",
                if vms == 1 { "microVM" } else { "microVMs" },
                if images == 1 { "image" } else { "images" },
                text::bytes(freed)
            ),
        )],
    );
    page.blank();
    if vms + images == 0 {
        page.line().pad(4).put(p, tokens::MUTED, "Nothing to remove.");
        return;
    }
    for i in (0..n).filter(|&i| sheet.get(i, "kind").is_none()) {
        let l = page.line();
        if let Some(name) = sheet.get(i, "vm") {
            l.pad(4)
                .put(p, tokens::SAGE, "● ")
                .put(p, tokens::FOREGROUND, name);
            l.pad(2)
                .put(p, tokens::SUBTLE, sheet.get(i, "id").unwrap_or(""))
                .pad(2)
                .put(p, tokens::MUTED, "microVM removed");
        } else if let Some(name) = sheet.get(i, "untagged") {
            l.pad(6)
                .put(p, tokens::SUBTLE, "untagged ")
                .put(p, tokens::FOREGROUND, name);
        } else if let Some(id) = sheet.get(i, "deleted") {
            l.pad(4)
                .put(p, tokens::SAGE, "● ")
                .put(p, tokens::FOREGROUND, id)
                .pad(2)
                .put(p, tokens::MUTED, "image removed");
        }
    }
}

/// `shards tag`: the new name, and the image it names.
fn tag(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let get = |k: &str| sheet.get(0, k).unwrap_or("");
    look::head(
        page,
        p,
        cols,
        "tag",
        &[(tokens::MUTED, false, format!("image {}", get("id")))],
    );
    page.blank();
    let l = page.line();
    l.pad(4)
        .put(p, tokens::SAGE, "◆ ")
        .bold(p, true)
        .put(p, tokens::BRIGHT, get("target"))
        .bold(p, false);
    l.pad(2)
        .put(p, tokens::MUTED, "now names what ")
        .put(p, tokens::FOREGROUND, get("source"))
        .put(p, tokens::MUTED, " does");
}

/// `shards port`: a microVM's published ports, each from the host to it.
fn port(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let target = sheet.get(0, "target").unwrap_or("");
    look::head(
        page,
        p,
        cols,
        "port",
        &[(tokens::BRIGHT, true, target.to_string())],
    );
    page.blank();
    for i in 1..sheet.records.len() {
        let Some(mapping) = sheet.get(i, "mapping") else {
            continue;
        };
        let l = page.line();
        l.pad(4);
        match mapping.split_once(" -> ") {
            Some((inside, outside)) => {
                l.put(p, tokens::TEAL, outside)
                    .put(p, tokens::SUBTLE, "  ──▸  ")
                    .put(p, tokens::FOREGROUND, inside);
            }
            None => {
                l.put(p, tokens::TEAL, mapping);
            }
        }
    }
}

/// JSON, coloured as the site colours code: keys in the foreground, strings sage,
/// numbers and literals lavender, punctuation subtle.
fn json(p: &Paint, text: &str) {
    let mut out = String::with_capacity(text.len() * 2);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::from('"');
                let mut escaped = false;
                for d in chars.by_ref() {
                    s.push(d);
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == '"' {
                        break;
                    }
                }
                // A key is a string a colon follows.
                let mut ahead = chars.clone();
                while ahead.peek().is_some_and(|c| *c == ' ') {
                    ahead.next();
                }
                let key = ahead.peek() == Some(&':');
                p.fg(&mut out, if key { tokens::FOREGROUND } else { tokens::SAGE });
                out.push_str(&s);
            }
            '{' | '}' | '[' | ']' | ':' | ',' => {
                p.fg(&mut out, tokens::SUBTLE);
                out.push(c);
            }
            c if c.is_ascii_digit() || c == '-' || c.is_ascii_alphabetic() => {
                p.fg(&mut out, tokens::LAVENDER);
                out.push(c);
                while let Some(d) = chars.peek().copied() {
                    if d.is_ascii_alphanumeric() || matches!(d, '.' | '-' | '+') {
                        out.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            c => out.push(c),
        }
    }
    p.reset(&mut out);
    out.push('\n');
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(out.as_bytes());
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// `s` as a terminal shows it: escapes taken out.
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

    /// Where each heading of the heading line starts, in columns.
    fn starts(heading: &str) -> Vec<usize> {
        let chars: Vec<char> = heading.chars().collect();
        (0..chars.len())
            .filter(|&i| chars[i] != ' ' && (i == 0 || chars[i - 1] == ' '))
            .collect()
    }

    /// A table drawn at `cols`: each row's cells start where their headings do, and
    /// every row's marker sits in one column, before them.
    fn aligned(columns: &[Column], rows: &[(Cell, Vec<Cell>)], cols: usize) {
        let mut page = Page::new();
        table(&mut page, &Paint::new(true), cols, columns, rows);
        let mut out = Vec::new();
        page.write(&Paint::new(true), &mut out);
        let text = String::from_utf8(out).unwrap();
        // The screen's last, empty line is no row.
        let lines: Vec<String> = text.lines().map(seen).filter(|l| !l.trim().is_empty()).collect();
        let heads = starts(&lines[0]);
        let marker_at = lines[1].chars().position(|c| c != ' ').unwrap();
        for (r, line) in lines.iter().enumerate().skip(1) {
            let chars: Vec<char> = line.chars().collect();
            assert_eq!(
                chars.iter().position(|c| *c != ' '),
                Some(marker_at),
                "row {r} at {cols}: {line:?}"
            );
            for &h in &heads {
                // A cell begins where its heading does: there, something; before it, a gap.
                assert!(
                    h < chars.len() && chars[h] != ' ' && chars[h - 1] == ' ',
                    "row {r}, heading at {h}, at {cols}:\n{}\n{line}",
                    lines[0]
                );
            }
            assert!(chars.len() <= cols, "row {r} wider than {cols}");
        }
    }

    #[test]
    fn tables_stand_in_their_columns_at_every_width() {
        let columns = [
            Column {
                heading: "NAME",
                right: false,
                keep: 9,
            },
            Column {
                heading: "TYPE",
                right: false,
                keep: 8,
            },
            Column {
                heading: "LAYERS",
                right: false,
                keep: 2,
            },
            Column {
                heading: "SIZE",
                right: false,
                keep: 7,
            },
            Column {
                heading: "RUNNING",
                right: false,
                keep: 6,
            },
        ];
        let row = |name: &str, kind: &str, layers: &str, size: &str, share: f64, running: &str| {
            let mut size = Cell::new(format!("{size:<7}"), tokens::LAVENDER);
            size.bar = Some((8, share));
            (
                Cell::new("●", tokens::SAGE),
                vec![
                    Cell::new(name, tokens::BRIGHT).bold(),
                    Cell::new(kind, tokens::SAGE),
                    Cell::new(layers, tokens::FOREGROUND),
                    size,
                    Cell::new(running, tokens::AMBER),
                ],
            )
        };
        let rows = [
            row("ubuntu:latest", "microVM", "2", "133 MB", 1.0, "● 3 running"),
            row("alpine:3.22", "image", "12", "8.85 MB", 0.06, "—"),
        ];
        for cols in [40, 60, 80, 120, 200] {
            aligned(&columns, &rows, cols);
        }
    }

    #[test]
    fn ages_read_well() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 3 * 86_400), "3 days ago");
        assert_eq!(ago(0), "—");
    }

    #[test]
    fn an_images_page_draws_every_image() {
        let mut sheet = Sheet::new("images");
        sheet.record(&[("kind", "head".into()), ("images", "2".into())]);
        sheet.record(&[
            ("repo", "node".into()),
            ("tag", "22-slim".into()),
            ("disk", "259000000".into()),
        ]);
        sheet.record(&[("repo", "alpine".into()), ("tag", "<none>".into())]);
        let mut page = Page::new();
        images(&mut page, &Paint::new(true), &sheet);
        let mut out = Vec::new();
        page.write(&Paint::new(true), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("node:22-slim") && text.contains("alpine") && text.contains("259 MB"));
    }
}
