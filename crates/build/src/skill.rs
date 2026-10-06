//! Agent Skills, as `SKILL` takes them (AGENTFILE_ARCH.md §8 Q12, architecture.md D54): a
//! directory whose `SKILL.md` opens with YAML frontmatter naming it and saying what it is
//! for. A skill is checked as the format's reference validator checks it (agentskills
//! skills-ref, `validate`; held to it by `tests/skills.rs` against what
//! `scripts/skills/generate` records it making of `testdata/skills`), with Python's own
//! string rules: lengths in code points, names NFKC-normalized, letters and digits as
//! `str.isalnum` knows them.
//!
//! The frontmatter is read as strictyaml reads it: YAML's block style alone, every scalar a
//! string; flow style, anchors, aliases, tags, merge keys and repeated keys refused.
//! strictyaml's own texts for those (ruamel's, with its parser's internals) are not kept:
//! each says what is wrong and where, after the reference validator's own prefix.

use unicode_normalization::UnicodeNormalization as _;

use crate::skill_tables::ALNUM;

/// The fields a `SKILL.md` may have (skills-ref ALLOWED_FIELDS), in order.
pub const ALLOWED: [&str; 6] = [
    "allowed-tools",
    "compatibility",
    "description",
    "license",
    "metadata",
    "name",
];
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;
const MAX_COMPATIBILITY: usize = 500;

/// A YAML node as strictyaml makes one: every scalar a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yaml {
    Str(String),
    Map(Vec<(String, Yaml)>),
    Seq(Vec<Yaml>),
}

/// strictyaml's tab rule: none outside a quoted or block value.
const TAB: &str = "a tab outside a quoted or block value, which YAML's structure forbids";

/// Whether `text` holds a tab outside the quoted value it may start with.
fn tab_outside_quotes(text: &str) -> bool {
    match text.as_bytes().first() {
        Some(&q @ (b'"' | b'\'')) => match quoted_end(text, q) {
            Some(e) => text.get(e..).is_some_and(|r| r.contains('\t')),
            None => false,
        },
        _ => text.contains('\t'),
    }
}

/// What makes the frontmatter unreadable, said after the reference validator's prefix.
fn yaml_error(line: usize, why: &str) -> String {
    format!("Invalid YAML in frontmatter: {why} (frontmatter line {line})")
}

/// `str.isspace`: whitespace as Python knows it, the information separators included.
fn py_space(c: char) -> bool {
    matches!(c, '\u{1c}'..='\u{1f}') || c.is_whitespace()
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

/// `str.isalnum` of one character, from the oracle's Python (skill_tables.rs).
fn py_alnum(c: char) -> bool {
    let c = u32::from(c);
    ALNUM
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// skills-ref's `parse_frontmatter`: the text between the first two `---`, wherever the
/// second is, read as a mapping; and the rest. The text is as Python reads a file: UTF-8,
/// its line ends made `\n`.
pub fn parse_frontmatter(content: &str) -> Result<(Vec<(String, Yaml)>, String), String> {
    if !content.starts_with("---") {
        return Err("SKILL.md must start with YAML frontmatter (---)".into());
    }
    let mut parts = content.splitn(3, "---");
    let (Some(_), Some(front), Some(body)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("SKILL.md frontmatter not properly closed with ---".into());
    };
    match parse_yaml(front)? {
        Yaml::Map(m) => Ok((m, py_strip(body).to_string())),
        _ => Err("SKILL.md frontmatter must be a YAML mapping".into()),
    }
}

/// skills-ref's `validate`, of a skill in the directory `dir` whose `SKILL.md` (or
/// `skill.md`) is `skill_md`, if it has one: what is wrong, or nothing.
pub fn validate(dir: &str, skill_md: Option<&[u8]>) -> Vec<String> {
    let Some(bytes) = skill_md else {
        return vec!["Missing required file: SKILL.md".into()];
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return vec!["SKILL.md is not UTF-8 text".into()];
    };
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    match parse_frontmatter(&text) {
        Err(e) => vec![e],
        Ok((metadata, _)) => validate_metadata(&metadata, Some(dir)),
    }
}

/// skills-ref's `validate_metadata`.
pub fn validate_metadata(metadata: &[(String, Yaml)], dir: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    let get = |k: &str| metadata.iter().find(|(key, _)| key == k).map(|(_, v)| v);
    let mut extra: Vec<&str> = metadata
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !ALLOWED.contains(k))
        .collect();
    extra.sort_unstable();
    extra.dedup();
    if !extra.is_empty() {
        errors.push(format!(
            "Unexpected fields in frontmatter: {}. Only ['allowed-tools', 'compatibility', 'description', 'license', 'metadata', 'name'] are allowed.",
            extra.join(", ")
        ));
    }
    match get("name") {
        None => errors.push("Missing required field in frontmatter: name".into()),
        Some(v) => errors.extend(validate_name(v, dir)),
    }
    match get("description") {
        None => errors.push("Missing required field in frontmatter: description".into()),
        Some(Yaml::Str(d)) if !py_strip(d).is_empty() => {
            let n = d.chars().count();
            if n > MAX_DESCRIPTION {
                errors.push(format!(
                    "Description exceeds {MAX_DESCRIPTION} character limit ({n} chars)"
                ));
            }
        }
        Some(_) => errors.push("Field 'description' must be a non-empty string".into()),
    }
    match get("compatibility") {
        None => {}
        Some(Yaml::Str(c)) => {
            let n = c.chars().count();
            if n > MAX_COMPATIBILITY {
                errors.push(format!(
                    "Compatibility exceeds {MAX_COMPATIBILITY} character limit ({n} chars)"
                ));
            }
        }
        Some(_) => errors.push("Field 'compatibility' must be a string".into()),
    }
    errors
}

/// The name a skill's frontmatter gives it, as `validate` reads it: stripped and
/// NFKC-normalized; none if it is no string or empty.
pub fn name_of(metadata: &[(String, Yaml)]) -> Option<String> {
    match metadata.iter().find(|(k, _)| k == "name").map(|(_, v)| v) {
        Some(Yaml::Str(n)) if !py_strip(n).is_empty() => Some(py_strip(n).nfkc().collect()),
        _ => None,
    }
}

fn validate_name(v: &Yaml, dir: Option<&str>) -> Vec<String> {
    let Yaml::Str(raw) = v else {
        return vec!["Field 'name' must be a non-empty string".into()];
    };
    if py_strip(raw).is_empty() {
        return vec!["Field 'name' must be a non-empty string".into()];
    }
    let name: String = py_strip(raw).nfkc().collect();
    let mut errors = Vec::new();
    let n = name.chars().count();
    if n > MAX_NAME {
        errors.push(format!(
            "Skill name '{name}' exceeds {MAX_NAME} character limit ({n} chars)"
        ));
    }
    if name != name.to_lowercase() {
        errors.push(format!("Skill name '{name}' must be lowercase"));
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("Skill name cannot start or end with a hyphen".into());
    }
    if name.contains("--") {
        errors.push("Skill name cannot contain consecutive hyphens".into());
    }
    if !name.chars().all(|c| c == '-' || py_alnum(c)) {
        errors.push(format!(
            "Skill name '{name}' contains invalid characters. Only letters, digits, and hyphens are allowed."
        ));
    }
    if let Some(dir) = dir {
        let dir_nfkc: String = dir.nfkc().collect();
        if dir_nfkc != name {
            errors.push(format!("Directory name '{dir}' must match skill name '{name}'"));
        }
    }
    errors
}

// ---------------------------------------------------------------------------------------
// strictyaml's YAML: block style, scalars as strings.

/// A line of the document: its number (from 1), its indentation and its text after it.
#[derive(Debug, Clone)]
struct Line<'a> {
    no: usize,
    indent: usize,
    text: &'a str,
}

/// Whether a line holds nothing but whitespace or a comment.
fn is_blank(text: &str) -> bool {
    let t = text.trim_start_matches([' ', '\t']);
    t.is_empty() || t.starts_with('#')
}

/// Parses `src` as one YAML document of strictyaml's subset.
pub fn parse_yaml(src: &str) -> Result<Yaml, String> {
    let mut lines = Vec::new();
    // What follows the last line break is a line only if it holds anything.
    let body = src.strip_suffix('\n').unwrap_or(src);
    for (i, raw) in body.split('\n').enumerate() {
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let text = raw.get(indent..).unwrap_or_default();
        // A document's end, and what follows it, is not read (`...`).
        if indent == 0 && (text == "..." || text.starts_with("... ")) {
            break;
        }
        lines.push(Line {
            no: i + 1,
            indent,
            text,
        });
    }
    let mut p = Parser { lines, at: 0 };
    p.skip_blank()?;
    // A document that is no collection is its text as written, as strictyaml gives it.
    let first = p.lines.get(p.at).map(|l| l.text);
    if first.is_none_or(|t| !is_seq_item(t) && entry_colon(t).is_none() && t != "?" && !t.starts_with("? ")) {
        if first.is_some() {
            p.node(None)?;
            p.skip_blank()?;
            if let Some(l) = p.lines.get(p.at) {
                return Err(yaml_error(
                    l.no,
                    "more after the document's end, at the wrong indentation",
                ));
            }
        }
        return Ok(Yaml::Str(src.to_string()));
    }
    let node = p.node(None)?;
    p.skip_blank()?;
    if let Some(l) = p.lines.get(p.at) {
        return Err(yaml_error(
            l.no,
            "more after the document's end, at the wrong indentation",
        ));
    }
    Ok(node)
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    at: usize,
}

/// Where a mapping entry's colon is in `text`, if the line is one: after a quoted key, or
/// the first `:` followed by a space or the line's end outside a comment.
fn entry_colon(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if let Some(&q @ (b'"' | b'\'')) = bytes.first() {
        let end = quoted_end(text, q)?;
        let rest = text.get(end..)?;
        let after = rest.trim_start_matches(' ');
        if after.starts_with(':')
            && (after.len() == 1 || after.as_bytes().get(1).is_some_and(|b| *b == b' ' || *b == b'\t'))
        {
            return Some(end + (rest.len() - after.len()));
        }
        return None;
    }
    let mut prev_space = true;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && prev_space {
            return None;
        }
        if b == b':' && bytes.get(i + 1).is_none_or(|n| *n == b' ' || *n == b'\t') {
            return Some(i);
        }
        prev_space = b == b' ' || b == b'\t';
    }
    None
}

/// The index past a quoted scalar's closing quote, on one line.
fn quoted_end(text: &str, q: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 1;
    while let Some(&b) = bytes.get(i) {
        if q == b'"' && b == b'\\' {
            i += 2;
            continue;
        }
        if b == q {
            if q == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ") || text.starts_with("-\t")
}

/// What strictyaml refuses at the start of a value.
fn refused(text: &str, line: usize) -> Result<(), String> {
    match text.chars().next() {
        Some('{' | '[') => Err(yaml_error(
            line,
            "flow style ({ } or [ ]), which strictyaml refuses: quote the text",
        )),
        Some('&') => Err(yaml_error(
            line,
            "an anchor (&), which strictyaml refuses: quote the text",
        )),
        Some('*') => Err(yaml_error(
            line,
            "an alias (*), which strictyaml refuses: quote the text",
        )),
        Some('!') => Err(yaml_error(
            line,
            "a tag (!), which strictyaml refuses: quote the text",
        )),
        Some('|' | '>') => Ok(()),
        Some('%' | '@' | '`') => Err(yaml_error(
            line,
            "a character that cannot start a plain value: quote the text",
        )),
        Some(c @ ('-' | '?' | ':'))
            if text.len() == 1 || text.as_bytes().get(1).is_some_and(|b| *b == b' ' || *b == b'\t') =>
        {
            Err(yaml_error(
                line,
                &format!(
                    "{c:?} followed by a space where a value is, which YAML reads as structure: quote the text"
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// A plain scalar's text on one line: up to a comment, trailing spaces dropped; refused if
/// it holds a mapping's `: `.
fn plain_part(text: &str, line: usize) -> Result<&str, String> {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    let mut prev_space = false;
    if text.contains('\t') {
        return Err(yaml_error(line, TAB));
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && prev_space {
            end = i;
            break;
        }
        if b == b':' && bytes.get(i + 1).is_none_or(|n| *n == b' ' || *n == b'\t') {
            return Err(yaml_error(
                line,
                "a mapping's \": \" inside a value: quote the text",
            ));
        }
        prev_space = b == b' ' || b == b'\t';
    }
    Ok(text.get(..end).unwrap_or_default().trim_end_matches([' ', '\t']))
}

/// The line breaks YAML knows besides `\n` (NEL, LINE and PARAGRAPH SEPARATOR): within a
/// plain value they fold as a break does, without ending the line.
fn fold_soft_breaks(part: &str) -> String {
    if !part.contains(['\u{85}', '\u{2028}', '\u{2029}']) {
        return part.to_string();
    }
    part.split(['\u{85}', '\u{2028}', '\u{2029}'])
        .map(|p| p.trim_matches([' ', '\t']))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl Parser<'_> {
    /// Passes blank and comment lines, none of which may hold a tab.
    fn skip_blank(&mut self) -> Result<(), String> {
        while let Some(l) = self.lines.get(self.at) {
            if !is_blank(l.text) {
                break;
            }
            if l.text.contains('\t') {
                return Err(yaml_error(l.no, TAB));
            }
            self.at += 1;
        }
        Ok(())
    }

    fn line_no(&self) -> usize {
        self.lines.get(self.at).or(self.lines.last()).map_or(0, |l| l.no)
    }

    /// The node starting at the current line, whose parent sits at `parent` (none: the
    /// document).
    fn node(&mut self, parent: Option<usize>) -> Result<Yaml, String> {
        self.skip_blank()?;
        let Some(l) = self.lines.get(self.at).cloned() else {
            return Ok(Yaml::Str(String::new()));
        };
        if is_seq_item(l.text) {
            return self.seq(l.indent);
        }
        if entry_colon(l.text).is_some() || l.text == "?" || l.text.starts_with("? ") {
            return self.map(l.indent);
        }
        let text = l.text;
        self.at += 1;
        self.scalar(text, l.no, parent)
    }

    fn map(&mut self, indent: usize) -> Result<Yaml, String> {
        let mut out: Vec<(String, Yaml)> = Vec::new();
        loop {
            self.skip_blank()?;
            let Some(l) = self.lines.get(self.at).cloned() else {
                break;
            };
            if l.indent < indent {
                break;
            }
            if l.indent > indent {
                return Err(yaml_error(l.no, "a line indented deeper than its mapping's keys"));
            }
            if let Some(colon) = entry_colon(l.text)
                && (l
                    .text
                    .get(..colon)
                    .is_some_and(|k| k.contains('\t') && !k.starts_with(['"', '\'']))
                    || tab_outside_quotes(
                        l.text
                            .get(colon + 1..)
                            .unwrap_or_default()
                            .trim_start_matches(' '),
                    ))
            {
                return Err(yaml_error(l.no, TAB));
            }
            let (key, rest) = if l.text == "?" || l.text.starts_with("? ") {
                // An explicit key, its value on a `:` line of the same indentation.
                let k = l.text.get(1..).unwrap_or_default().trim();
                let key = self.key_text(k, l.no)?;
                self.at += 1;
                self.skip_blank()?;
                match self.lines.get(self.at).cloned() {
                    Some(v) if v.indent == indent && (v.text == ":" || v.text.starts_with(": ")) => (
                        key,
                        v.text
                            .get(1..)
                            .unwrap_or_default()
                            .trim_start_matches([' ', '\t'])
                            .to_string(),
                    ),
                    _ => (key, String::new()),
                }
            } else {
                let Some(colon) = entry_colon(l.text) else {
                    return Err(yaml_error(
                        l.no,
                        "a line that is no \"key: value\" among a mapping's",
                    ));
                };
                let key = self.key_text(l.text.get(..colon).unwrap_or_default().trim_end(), l.no)?;
                (
                    key,
                    l.text
                        .get(colon + 1..)
                        .unwrap_or_default()
                        .trim_start_matches([' ', '\t'])
                        .to_string(),
                )
            };
            if key == "<<" {
                return Err(yaml_error(l.no, "a merge key (<<), which strictyaml refuses"));
            }
            if out.iter().any(|(k, _)| *k == key) {
                return Err(yaml_error(l.no, &format!("the key {key:?} given twice")));
            }
            let no = self.line_no();
            self.at += 1;
            let rest_trim = if rest.starts_with('#') { "" } else { rest.as_str() };
            let value = if rest_trim.is_empty() {
                // A value on the lines below: deeper, or a list at the key's indentation.
                self.skip_blank()?;
                match self.lines.get(self.at).cloned() {
                    Some(n) if n.indent > indent => self.node(Some(indent))?,
                    Some(n) if n.indent == indent && is_seq_item(n.text) => self.seq(indent)?,
                    _ => Yaml::Str(String::new()),
                }
            } else {
                let text = rest_trim.to_string();
                self.inline_value(&text, no, indent)?
            };
            out.push((key, value));
        }
        Ok(Yaml::Map(out))
    }

    /// A key, plain or quoted.
    fn key_text(&self, k: &str, line: usize) -> Result<String, String> {
        refused(k, line)?;
        match k.as_bytes().first() {
            Some(b'"') => double_quoted(k.get(1..k.len().saturating_sub(1)).unwrap_or_default(), line),
            Some(b'\'') => Ok(single_quoted(
                k.get(1..k.len().saturating_sub(1)).unwrap_or_default(),
            )),
            _ => Ok(k.to_string()),
        }
    }

    /// A value that starts on its key's line.
    fn inline_value(&mut self, text: &str, line: usize, indent: usize) -> Result<Yaml, String> {
        refused(text, line)?;
        self.scalar(text, line, Some(indent))
    }

    fn seq(&mut self, indent: usize) -> Result<Yaml, String> {
        let mut out = Vec::new();
        loop {
            self.skip_blank()?;
            let Some(l) = self.lines.get(self.at).cloned() else {
                break;
            };
            if l.indent != indent || !is_seq_item(l.text) {
                if l.indent > indent {
                    return Err(yaml_error(l.no, "a line indented deeper than its list's items"));
                }
                break;
            }
            let rest = l.text.get(1..).unwrap_or_default();
            if rest.trim_start_matches(' ').starts_with('\t') {
                return Err(yaml_error(l.no, TAB));
            }
            let pad = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            let item = rest.trim_start_matches([' ', '\t']);
            if item.is_empty() || item.starts_with('#') {
                self.at += 1;
                self.skip_blank()?;
                match self.lines.get(self.at).cloned() {
                    Some(n) if n.indent > indent => out.push(self.node(Some(indent))?),
                    _ => out.push(Yaml::Str(String::new())),
                }
                continue;
            }
            // The item's text as a line of its own, indented where it starts.
            if let Some(cur) = self.lines.get_mut(self.at) {
                cur.indent = indent + 1 + pad;
                cur.text = item;
            }
            out.push(self.node(Some(indent))?);
        }
        Ok(Yaml::Seq(out))
    }

    /// A scalar starting with `first` (on line `line`, already taken), its parent at
    /// `parent`: block, quoted or plain, with the lines that continue it.
    fn scalar(&mut self, first: &str, line: usize, parent: Option<usize>) -> Result<Yaml, String> {
        refused(first, line)?;
        let min = parent.map_or(0, |p| p + 1);
        match first.as_bytes().first() {
            Some(b'|' | b'>') => self.block(first, line, parent),
            Some(&q @ (b'"' | b'\'')) => self.quoted(first, q, line, min),
            _ => {
                let mut out = fold_soft_breaks(plain_part(first, line)?);
                let mut breaks = 0usize;
                while let Some(l) = self.lines.get(self.at).cloned() {
                    if l.text.trim_start_matches([' ', '\t']).is_empty() {
                        if l.text.contains('\t') {
                            return Err(yaml_error(l.no, TAB));
                        }
                        breaks += 1;
                        self.at += 1;
                        continue;
                    }
                    if l.indent < min || l.text.starts_with('#') {
                        break;
                    }
                    if parent.is_none() && l.indent == 0 && is_seq_item(l.text) {
                        break;
                    }
                    let part = fold_soft_breaks(plain_part(l.text, l.no)?);
                    if breaks == 0 {
                        out.push(' ');
                    } else {
                        out.push_str(&"\n".repeat(breaks));
                    }
                    breaks = 0;
                    out.push_str(&part);
                    self.at += 1;
                    if l.text.contains(" #") {
                        break;
                    }
                }
                // Blank lines after a plain value are not part of it.
                self.at -= breaks.min(self.at);
                Ok(Yaml::Str(out))
            }
        }
    }

    fn quoted(&mut self, first: &str, q: u8, line: usize, min: usize) -> Result<Yaml, String> {
        // The text up to the closing quote, across lines if it goes on.
        let mut raw = first.get(1..).unwrap_or_default().to_string();
        let mut close = quoted_end(first, q);
        while close.is_none() {
            let Some(l) = self.lines.get(self.at).cloned() else {
                return Err(yaml_error(line, "a quoted value never closed"));
            };
            if !l.text.trim().is_empty() && l.indent < min {
                return Err(yaml_error(l.no, "a quoted value's line indented too little"));
            }
            self.at += 1;
            raw.push('\n');
            raw.push_str(l.text);
            let probe = format!("{}{}", q as char, raw);
            close = quoted_end(&probe, q).map(|e| e - 1);
            if let Some(e) = close {
                let after = probe.get(e + 1..).unwrap_or_default();
                return self.finish_quoted(probe.get(1..e).unwrap_or_default(), after, q, l.no);
            }
        }
        let e = close.unwrap_or(first.len());
        let after = first.get(e..).unwrap_or_default();
        self.finish_quoted(
            first.get(1..e.saturating_sub(1)).unwrap_or_default(),
            after,
            q,
            line,
        )
    }

    fn finish_quoted(&self, inner: &str, after: &str, q: u8, line: usize) -> Result<Yaml, String> {
        if after.contains('\t') {
            return Err(yaml_error(line, TAB));
        }
        let after = after.trim_start_matches(' ');
        if !after.is_empty() && !after.starts_with('#') {
            return Err(yaml_error(line, "text after a quoted value's closing quote"));
        }
        let folded = fold_quoted(inner);
        Ok(Yaml::Str(if q == b'"' {
            double_quoted(&folded, line)?
        } else {
            single_quoted(&folded)
        }))
    }

    /// A block scalar, `|` or `>`, with its chomping and indentation indicators.
    fn block(&mut self, header: &str, line: usize, parent: Option<usize>) -> Result<Yaml, String> {
        let literal = header.starts_with('|');
        let mut keep = None;
        let mut explicit = None;
        let head = header.get(1..).unwrap_or_default();
        let head = head.split(" #").next().unwrap_or_default().trim_end();
        for c in head.chars() {
            match c {
                '+' if keep.is_none() => keep = Some(true),
                '-' if keep.is_none() => keep = Some(false),
                '1'..='9' if explicit.is_none() => explicit = c.to_digit(10).map(|d| d as usize),
                _ => {
                    return Err(yaml_error(
                        line,
                        "a block scalar's header holds more than | or >, + or -, and a digit",
                    ));
                }
            }
        }
        let base = parent.map_or(0, |p| p + 1);
        let mut content_indent = explicit.map(|e| parent.unwrap_or(0) + e);
        let mut body: Vec<(usize, &str)> = Vec::new();
        while let Some(l) = self.lines.get(self.at).cloned() {
            let empty = l.text.trim_start_matches(' ').is_empty();
            if !empty {
                let ci = *content_indent.get_or_insert(l.indent);
                if l.indent < ci || l.indent < base {
                    break;
                }
            }
            body.push((l.indent, l.text));
            self.at += 1;
        }
        let ci = content_indent.unwrap_or(base);
        // Trailing blank lines belong to the chomping, not the text.
        let mut trailing = 0;
        while body
            .last()
            .is_some_and(|(_, t)| t.trim_start_matches(' ').is_empty())
        {
            body.pop();
            trailing += 1;
        }
        let lines: Vec<String> = body
            .iter()
            .map(|(ind, t)| {
                if t.trim_start_matches(' ').is_empty() {
                    String::new()
                } else {
                    format!("{}{}", " ".repeat(ind.saturating_sub(ci)), t)
                }
            })
            .collect();
        let mut out = String::new();
        if literal {
            out = lines.join("\n");
        } else {
            let mut prev_more = false;
            let mut started = false;
            let mut pending_breaks = 0usize;
            for l in &lines {
                if l.is_empty() {
                    pending_breaks += 1;
                    continue;
                }
                let more = l.starts_with(' ') || l.starts_with('\t');
                if started {
                    if pending_breaks == 0 && !more && !prev_more {
                        out.push(' ');
                    } else if pending_breaks == 0 {
                        out.push('\n');
                    } else {
                        let n = if more || prev_more {
                            pending_breaks + 1
                        } else {
                            pending_breaks
                        };
                        out.push_str(&"\n".repeat(n));
                    }
                } else if pending_breaks > 0 {
                    out.push_str(&"\n".repeat(pending_breaks));
                }
                pending_breaks = 0;
                out.push_str(l);
                prev_more = more;
                started = true;
            }
        }
        match keep {
            Some(false) => {}
            Some(true) => {
                if !lines.is_empty() {
                    out.push('\n');
                }
                out.push_str(&"\n".repeat(trailing));
            }
            None => {
                if !lines.is_empty() {
                    out.push('\n');
                }
            }
        }
        Ok(Yaml::Str(out))
    }
}

/// A quoted value's line breaks folded: a break between text is a space; each empty line
/// a newline; each line's leading and trailing spaces dropped.
fn fold_quoted(inner: &str) -> String {
    if !inner.contains('\n') {
        return inner.to_string();
    }
    let parts: Vec<&str> = inner.split('\n').collect();
    let last = parts.len() - 1;
    let mut out = String::new();
    let mut empties = 0usize;
    for (i, p) in parts.iter().enumerate() {
        let t = if i == 0 {
            p.trim_end_matches([' ', '\t'])
        } else if i == last {
            p.trim_start_matches([' ', '\t'])
        } else {
            p.trim_matches([' ', '\t'])
        };
        if i > 0 && t.is_empty() && i != last {
            empties += 1;
            continue;
        }
        if i > 0 {
            if empties == 0 {
                // An escaped line break joins without a space.
                if out.ends_with('\\') && !out.ends_with("\\\\") {
                    out.pop();
                } else {
                    out.push(' ');
                }
            } else {
                out.push_str(&"\n".repeat(empties));
            }
            empties = 0;
        }
        out.push_str(t);
    }
    out
}

fn single_quoted(inner: &str) -> String {
    inner.replace("''", "'")
}

fn double_quoted(inner: &str, line: usize) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = chars.next() else {
            return Err(yaml_error(line, "a quoted value ending in \\"));
        };
        let hex = |chars: &mut std::str::Chars<'_>, n: usize| -> Result<char, String> {
            let h: String = chars.take(n).collect();
            u32::from_str_radix(&h, 16)
                .ok()
                .filter(|_| h.len() == n)
                .and_then(char::from_u32)
                .ok_or_else(|| yaml_error(line, &format!("the escape \\{e}{h}, which names no character")))
        };
        out.push(match e {
            '0' => '\0',
            'a' => '\u{7}',
            'b' => '\u{8}',
            't' | '\t' => '\t',
            'n' => '\n',
            'v' => '\u{b}',
            'f' => '\u{c}',
            'r' => '\r',
            'e' => '\u{1b}',
            ' ' => ' ',
            '"' => '"',
            '/' => '/',
            '\\' => '\\',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            'x' => hex(&mut chars, 2)?,
            'u' => hex(&mut chars, 4)?,
            'U' => hex(&mut chars, 8)?,
            other => return Err(yaml_error(line, &format!("the unknown escape \\{other}"))),
        });
    }
    Ok(out)
}
