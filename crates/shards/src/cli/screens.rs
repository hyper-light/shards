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

/// `shards images`: the store's totals, then each image a row: whether it boots here,
/// its name, ID and age, its layers, its microVM's disk drawn against the largest, and
/// its platform; the columns giving way, least needed first, where the terminal is
/// narrow.
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
    if rows.is_empty() {
        page.blank();
        let l = page.line();
        l.pad(4).put(p, tokens::MUTED, "No images yet. ");
        l.put(p, tokens::TEAL, "shards pull IMAGE");
        l.put(p, tokens::MUTED, " brings one, and makes it a microVM.");
        return;
    }
    let name = |i: usize| -> String {
        let repo = sheet.get(i, "repo").unwrap_or("<none>");
        match sheet.get(i, "tag").filter(|t| *t != "<none>") {
            Some(tag) => format!("{repo}:{tag}"),
            None => repo.to_string(),
        }
    };
    let name_w = rows
        .iter()
        .map(|&i| name(i).chars().count())
        .max()
        .unwrap_or(0)
        .clamp(5, 40);
    let largest = rows.iter().map(|&i| num(i, "disk")).max().unwrap_or(0).max(1);
    // Columns: (header, width), and whether each fits.
    const ID: usize = 12;
    const AGE: usize = 14;
    const LAYERS: usize = 6;
    const SIZE: usize = 8;
    let platform_w = rows
        .iter()
        .map(|&i| sheet.get(i, "platform").unwrap_or("").len())
        .max()
        .unwrap_or(0)
        .max(8);
    let fixed = 6 + name_w + 2 + ID + 2 + AGE;
    let with_layers = cols >= fixed + 2 + LAYERS + 2 + SIZE;
    let with_platform = cols >= fixed + 2 + LAYERS + 2 + SIZE + 2 + 10 + 2 + platform_w;
    let bar_w = cols
        .saturating_sub(
            fixed + 2 + LAYERS + 2 + SIZE + 2 + if with_platform { 2 + platform_w } else { 0 } + 2,
        )
        .min(24);
    let with_bar = with_layers && bar_w >= 6;
    page.blank();
    {
        let l = page.line();
        l.pad(6);
        let h = |l: &mut look::Line, w: usize, words: &str| {
            l.put(p, tokens::EYEBROW, &words.to_uppercase())
                .pad(w.saturating_sub(words.len()) + 2);
        };
        h(l, name_w, "image");
        h(l, ID, "id");
        h(l, AGE, "created");
        if with_layers {
            l.put(p, tokens::EYEBROW, "LAYERS").pad(2);
            if with_bar {
                h(l, bar_w, "microvm disk");
            } else {
                l.pad(2);
            }
            l.pad(SIZE + 2);
        }
        if with_platform {
            l.put(p, tokens::EYEBROW, "PLATFORM");
        }
    }
    for (k, &i) in rows.iter().enumerate() {
        let disk = num(i, "disk");
        let in_use = num(i, "in_use");
        let l = page.line();
        l.pad(4);
        // Ready to boot, sage; stored for another platform, grey.
        let (glyph, c) = if disk > 0 {
            ("◆", tokens::SAGE)
        } else {
            ("◇", tokens::SUBTLE)
        };
        l.put(p, c, glyph).pad(1);
        let shown = layout::clip(&name(i), name_w);
        let n = shown.chars().count();
        l.bold(p, true).put(p, tokens::BRIGHT, &shown).bold(p, false);
        l.pad(name_w - n + 2);
        l.put(p, tokens::SUBTLE, sheet.get(i, "id").unwrap_or("")).pad(2);
        let age = ago(sheet.get(i, "created").and_then(|c| c.parse().ok()).unwrap_or(0));
        let a = layout::clip(&age, AGE);
        l.put(p, tokens::MUTED, &a).pad(AGE - a.chars().count() + 2);
        if with_layers {
            let layers = num(i, "layers").to_string();
            l.pad(LAYERS - layers.len().min(LAYERS))
                .put(p, tokens::FOREGROUND, &layers)
                .pad(2);
            if with_bar {
                let w = ((disk as f64 / largest as f64) * bar_w as f64).round().max(1.0) as usize;
                if disk > 0 {
                    bar::draw(&mut l.s, p, w, Fill::Done, 0.0, k as f64 * 0.37);
                    l.w += w;
                }
                l.pad(bar_w - w.min(bar_w) + 2);
            } else {
                l.pad(2);
            }
            let size = if disk > 0 {
                text::bytes(disk)
            } else {
                "stored".into()
            };
            l.pad(SIZE.saturating_sub(size.len()))
                .put(
                    p,
                    if disk > 0 {
                        tokens::LAVENDER
                    } else {
                        tokens::SUBTLE
                    },
                    &size,
                )
                .pad(2);
        }
        if with_platform {
            l.put(p, tokens::MUTED, sheet.get(i, "platform").unwrap_or(""));
        }
        if in_use > 0 {
            l.pad(2).put(p, tokens::AMBER, &format!("● {in_use} running"));
        }
    }
}

/// `shards ps`: how many run, then each microVM a row: its state, name and ID, image,
/// how it stands, its ports and what it runs; the columns giving way, least needed
/// first, where the terminal is narrow.
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
                words.push_str(" (shards ps -a lists them)");
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
        l.put(p, tokens::TEAL, "shards run IMAGE");
        l.put(p, tokens::MUTED, " starts a microVM from an image.");
        return;
    }
    let width = |k: &str, least: usize, most: usize| -> usize {
        rows.iter()
            .map(|&i| sheet.get(i, k).unwrap_or("").chars().count())
            .max()
            .unwrap_or(0)
            .clamp(least, most)
    };
    let (name_w, image_w, status_w, ports_w) = (
        width("name", 4, 28),
        width("image", 5, 32),
        width("status", 6, 28),
        width("ports", 0, 34),
    );
    const ID: usize = 12;
    let fixed = 6 + name_w + 2 + ID + 2 + image_w + 2 + status_w;
    let with_ports = ports_w > 0 && cols >= fixed + 2 + ports_w;
    let command_w = cols.saturating_sub(fixed + if with_ports { 2 + ports_w } else { 0 } + 2);
    let with_command = command_w >= 10;
    {
        let l = page.line();
        l.pad(6);
        for (words, w) in [
            ("microvm", name_w),
            ("id", ID),
            ("image", image_w),
            ("status", status_w),
        ] {
            l.put(p, tokens::EYEBROW, &words.to_uppercase())
                .pad(w.saturating_sub(words.len()) + 2);
        }
        if with_ports {
            l.put(p, tokens::EYEBROW, "PORTS")
                .pad(ports_w.saturating_sub(5) + 2);
        }
        if with_command {
            l.put(p, tokens::EYEBROW, "RUNS");
        }
    }
    for &i in &rows {
        let state = sheet.get(i, "state").unwrap_or("");
        let failed = state == "exited" && sheet.get(i, "exit").is_some_and(|e| e != "0" && !e.is_empty());
        // Running, sage; created, lavender; ended well, grey; ended badly, rose.
        let (glyph, c) = match state {
            "running" => ("●", tokens::SAGE),
            "created" => ("◌", tokens::LAVENDER),
            _ if failed => ("○", tokens::ROSE),
            _ => ("○", tokens::SUBTLE),
        };
        let cell = |l: &mut look::Line, text: &str, w: usize, c| {
            let shown = layout::clip(text, w);
            let n = shown.chars().count();
            l.put(p, c, &shown).pad(w - n.min(w) + 2);
        };
        let l = page.line();
        l.pad(4).put(p, c, glyph).pad(1);
        l.bold(p, true);
        cell(l, sheet.get(i, "name").unwrap_or(""), name_w, tokens::BRIGHT);
        l.bold(p, false);
        cell(l, sheet.get(i, "id").unwrap_or(""), ID, tokens::SUBTLE);
        cell(
            l,
            sheet.get(i, "image").unwrap_or(""),
            image_w,
            tokens::FOREGROUND,
        );
        let status_c = match state {
            "running" => tokens::SAGE,
            _ if failed => tokens::ROSE,
            _ => tokens::MUTED,
        };
        cell(l, sheet.get(i, "status").unwrap_or(""), status_w, status_c);
        if with_ports {
            cell(l, sheet.get(i, "ports").unwrap_or(""), ports_w, tokens::TEAL);
        }
        if with_command {
            let command = sheet.get(i, "command").unwrap_or("").trim_matches('"');
            l.put(p, tokens::MUTED, &layout::clip(command, command_w));
        }
    }
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

/// `shards rmi`: each name untagged, each image deleted and what it frees, each refusal.
fn rmi(page: &mut Page, p: &Paint, sheet: &Sheet) {
    let cols = look::width();
    let n = sheet.records.len();
    let deleted = (0..n).filter(|&i| sheet.get(i, "deleted").is_some()).count();
    let untagged = (0..n).filter(|&i| sheet.get(i, "untagged").is_some()).count();
    let freed: u64 = (0..n)
        .filter_map(|i| sheet.get(i, "freed").and_then(|f| f.parse::<u64>().ok()))
        .sum();
    let mut summary = format!(
        "{deleted} {} deleted · {untagged} {} untagged",
        if deleted == 1 { "image" } else { "images" },
        if untagged == 1 { "name" } else { "names" }
    );
    if freed > 0 {
        summary.push_str(&format!(" · {} to free", text::bytes(freed)));
    }
    look::head(page, p, cols, "rmi", &[(tokens::MUTED, false, summary)]);
    page.blank();
    for i in 0..n {
        let l = page.line();
        l.pad(4);
        if let Some(name) = sheet.get(i, "untagged") {
            l.put(p, tokens::SUBTLE, "◇ ").put(p, tokens::FOREGROUND, name);
            l.pad(2).put(p, tokens::MUTED, "untagged");
        } else if let Some(id) = sheet.get(i, "deleted") {
            l.put(p, tokens::SAGE, "◆ ").put(p, tokens::BRIGHT, id);
            l.pad(2).put(p, tokens::SAGE, "deleted");
            if let Some(f) = sheet
                .get(i, "freed")
                .and_then(|f| f.parse::<u64>().ok())
                .filter(|f| *f > 0)
            {
                l.pad(2).put(
                    p,
                    tokens::MUTED,
                    &format!("{} freed with its microVM disk", text::bytes(f)),
                );
            }
        } else if let Some(e) = sheet.get(i, "error") {
            l.put(p, tokens::ROSE, "○ ")
                .put(p, tokens::BRIGHT, sheet.get(i, "given").unwrap_or(""));
            let room = cols.saturating_sub(l.w + 2);
            l.pad(2).put(p, tokens::ROSE, &layout::clip(e, room));
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
