//! The Dockerfile frontend's subrequests, as `build --call` asks them (moby/buildkit
//! dockerfile/1.27.1 frontend/subrequests, dockerui's HandleSubrequest): a target's
//! outline (its build arguments, secrets and SSH agents), the file's targets, and the
//! subrequests themselves; each its `result.json`, as `json.MarshalIndent` writes it, and
//! the text buildx prints of it (`PrintOutline`, `PrintTargets`, `PrintDescribe`, through
//! Go's text/tabwriter).

use crate::instructions::Location;
use crate::json;

/// `outline.Arg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    pub name: Vec<u8>,
    pub description: Vec<u8>,
    pub value: Vec<u8>,
    pub location: Location,
}

/// `outline.Secret` and `outline.SSH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub name: Vec<u8>,
    pub required: bool,
    pub location: Location,
}

/// `outline.Outline`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outline {
    pub name: Vec<u8>,
    pub description: Vec<u8>,
    pub args: Vec<Arg>,
    pub secrets: Vec<Mount>,
    pub ssh: Vec<Mount>,
    pub sources: Vec<Vec<u8>>,
}

/// `targets.Target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: Vec<u8>,
    pub default: bool,
    pub description: Vec<u8>,
    pub base: Vec<u8>,
    pub platform: Vec<u8>,
    pub location: Location,
}

/// `targets.List`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    pub targets: Vec<Target>,
    pub sources: Vec<Vec<u8>>,
}

/// `json.MarshalIndent(v, "", "  ")`'s object: fields in order, each already a value.
struct Object<'a> {
    out: &'a mut String,
    depth: usize,
    first: bool,
}

impl<'a> Object<'a> {
    fn open(out: &'a mut String, depth: usize) -> Object<'a> {
        out.push('{');
        Object {
            out,
            depth,
            first: true,
        }
    }

    fn key(&mut self, k: &str) -> &mut String {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        self.out.push('\n');
        indent(self.out, self.depth + 1);
        json::write_string(self.out, k.as_bytes());
        self.out.push_str(": ");
        self.out
    }

    fn string(&mut self, k: &str, v: &[u8]) {
        let out = self.key(k);
        json::write_string(out, v);
    }

    fn close(self) {
        if !self.first {
            self.out.push('\n');
            indent(self.out, self.depth);
        }
        self.out.push('}');
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// An array at `depth`, each item written by `item` at `depth + 1`; `[]` when empty.
fn array<T>(out: &mut String, depth: usize, items: &[T], item: impl Fn(&mut String, usize, &T)) {
    if items.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push('[');
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\n');
        indent(out, depth + 1);
        item(out, depth + 1, it);
    }
    out.push('\n');
    indent(out, depth);
    out.push(']');
}

/// `*pb.Location` as encoding/json writes it: `ranges`, each `start` and `end` with their
/// `line` (a character, always 0 for an instruction, left out, as is the source index).
fn location(out: &mut String, depth: usize, loc: &Location) {
    let mut o = Object::open(out, depth);
    let ranges = o.key("ranges");
    array(ranges, depth + 1, loc, |out, depth, (start, end)| {
        let mut r = Object::open(out, depth);
        for (k, line) in [("start", start), ("end", end)] {
            let at = r.key(k);
            let mut p = Object::open(at, depth + 1);
            if *line != 0 {
                let l = p.key("line");
                l.push_str(&line.to_string());
            }
            p.close();
        }
        r.close();
    });
    o.close();
}

fn sources(o: &mut Object<'_>, sources: &[Vec<u8>]) {
    let depth = o.depth + 1;
    let out = o.key("sources");
    array(out, depth, sources, |out, _, s| {
        json::write_string(out, base64(s).as_bytes())
    });
}

/// `[]byte` as encoding/json writes it: standard base64, padded.
fn base64(b: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | u32::from(*byte) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                let sextet = (n >> (18 - 6 * i)) & 63;
                out.push(char::from(ALPHABET.get(sextet as usize).copied().unwrap_or(b'=')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Outline {
    /// Its `result.json`.
    pub fn json(&self) -> String {
        let mut out = String::new();
        let mut o = Object::open(&mut out, 0);
        if !self.name.is_empty() {
            o.string("name", &self.name);
        }
        if !self.description.is_empty() {
            o.string("description", &self.description);
        }
        if !self.args.is_empty() {
            let a = o.key("args");
            array(a, 1, &self.args, |out, depth, arg| {
                let mut x = Object::open(out, depth);
                x.string("name", &arg.name);
                if !arg.description.is_empty() {
                    x.string("description", &arg.description);
                }
                if !arg.value.is_empty() {
                    x.string("value", &arg.value);
                }
                if !arg.location.is_empty() {
                    let l = x.key("location");
                    location(l, depth + 1, &arg.location);
                }
                x.close();
            });
        }
        for (k, mounts) in [("secrets", &self.secrets), ("ssh", &self.ssh)] {
            if mounts.is_empty() {
                continue;
            }
            let m = o.key(k);
            array(m, 1, mounts, |out, depth, mount| {
                let mut x = Object::open(out, depth);
                x.string("name", &mount.name);
                if mount.required {
                    x.key("required").push_str("true");
                }
                if !mount.location.is_empty() {
                    let l = x.key("location");
                    location(l, depth + 1, &mount.location);
                }
                x.close();
            });
        }
        if !self.sources.is_empty() {
            sources(&mut o, &self.sources);
        }
        o.close();
        out
    }

    /// `PrintOutline`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let shown = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        if !self.name.is_empty() || !self.description.is_empty() {
            let name = if self.name.is_empty() {
                "(default)".to_string()
            } else {
                shown(&self.name)
            };
            let mut rows = vec![vec!["TARGET:".to_string(), name]];
            if !self.description.is_empty() {
                rows.push(vec!["DESCRIPTION:".into(), shown(&self.description)]);
            }
            out.push_str(&tabwrite(&rows, 1));
            out.push('\n');
        }
        if !self.args.is_empty() {
            let mut rows = vec![vec![
                "BUILD ARG".to_string(),
                "VALUE".into(),
                "DESCRIPTION".into(),
            ]];
            for a in &self.args {
                rows.push(vec![shown(&a.name), shown(&a.value), shown(&a.description)]);
            }
            out.push_str(&tabwrite(&rows, 3));
            out.push('\n');
        }
        for (head, mounts) in [("SECRET", &self.secrets), ("SSH", &self.ssh)] {
            if mounts.is_empty() {
                continue;
            }
            let mut rows = vec![vec![head.to_string(), "REQUIRED".into()]];
            for m in mounts {
                rows.push(vec![shown(&m.name), if m.required { "true" } else { "" }.into()]);
            }
            out.push_str(&tabwrite(&rows, 3));
            out.push('\n');
        }
        out
    }
}

impl Targets {
    /// Its `result.json`.
    pub fn json(&self) -> String {
        let mut out = String::new();
        let mut o = Object::open(&mut out, 0);
        let t = o.key("targets");
        if self.targets.is_empty() {
            t.push_str("null");
        } else {
            array(t, 1, &self.targets, |out, depth, target| {
                let mut x = Object::open(out, depth);
                if !target.name.is_empty() {
                    x.string("name", &target.name);
                }
                if target.default {
                    x.key("default").push_str("true");
                }
                if !target.description.is_empty() {
                    x.string("description", &target.description);
                }
                if !target.base.is_empty() {
                    x.string("base", &target.base);
                }
                if !target.platform.is_empty() {
                    x.string("platform", &target.platform);
                }
                if !target.location.is_empty() {
                    let l = x.key("location");
                    location(l, depth + 1, &target.location);
                }
                x.close();
            });
        }
        if self.sources.is_empty() {
            o.key("sources").push_str("null");
        } else {
            sources(&mut o, &self.sources);
        }
        o.close();
        out
    }

    /// `PrintTargets`.
    pub fn text(&self) -> String {
        let mut rows = vec![vec!["TARGET".to_string(), "DESCRIPTION".into()]];
        for t in &self.targets {
            let name = String::from_utf8_lossy(&t.name).into_owned();
            let name = match (name.is_empty(), t.default) {
                (true, true) => "(default)".to_string(),
                (false, true) => format!("{name} (default)"),
                _ => name,
            };
            rows.push(vec![name, String::from_utf8_lossy(&t.description).into_owned()]);
        }
        tabwrite(&rows, 1)
    }
}

/// The subrequests the Dockerfile frontend answers (dockerui's describe, Outline and
/// ListTargets set): `result.json`.
pub const DESCRIBE: &str = r#"[
  {
    "name": "frontend.outline",
    "version": "1.0.0",
    "type": "rpc",
    "description": "List all parameters current build target supports",
    "opts": [
      {
        "name": "target",
        "description": "Target build stage"
      }
    ],
    "inputs": null,
    "metadata": [
      {
        "name": "result.json",
        "description": ""
      },
      {
        "name": "result.txt",
        "description": ""
      }
    ],
    "refs": null
  },
  {
    "name": "frontend.targets",
    "version": "1.0.0",
    "type": "rpc",
    "description": "List all targets current build supports",
    "opts": [],
    "inputs": null,
    "metadata": [
      {
        "name": "result.json",
        "description": ""
      },
      {
        "name": "result.txt",
        "description": ""
      }
    ],
    "refs": null
  },
  {
    "name": "frontend.subrequests.describe",
    "version": "1.0.0",
    "type": "rpc",
    "description": "List available subrequest types",
    "opts": null,
    "inputs": null,
    "metadata": [
      {
        "name": "result.json",
        "description": ""
      },
      {
        "name": "result.txt",
        "description": ""
      }
    ],
    "refs": null
  }
]"#;

/// `PrintDescribe` of [`DESCRIBE`].
pub fn describe_text() -> String {
    tabwrite(
        &[
            vec!["NAME".into(), "VERSION".into(), "DESCRIPTION".into()],
            vec![
                "outline".into(),
                "1.0.0".into(),
                "List all parameters current build target supports".into(),
            ],
            vec![
                "targets".into(),
                "1.0.0".into(),
                "List all targets current build supports".into(),
            ],
            vec![
                "subrequests.describe".into(),
                "1.0.0".into(),
                "List available subrequest types".into(),
            ],
        ],
        1,
    )
}

/// Go's text/tabwriter (minwidth 0, tabwidth 0, `padding`, spaces, no flags) of rows
/// whose every cell but the last ends in a tab: each such column as wide as its widest
/// cell, in runes, and `padding` more; the last cell as it is.
fn tabwrite(rows: &[Vec<String>], padding: usize) -> String {
    let columns = rows.iter().map(|r| r.len().saturating_sub(1)).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (i, cell) in row.iter().take(row.len().saturating_sub(1)).enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count() + padding);
            }
        }
    }
    let mut out = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        for (i, cell) in row.iter().enumerate() {
            out.push_str(cell);
            if i < last {
                let w = widths.get(i).copied().unwrap_or(0);
                out.push_str(&" ".repeat(w.saturating_sub(cell.chars().count())));
            }
        }
        out.push('\n');
    }
    out
}

/// `lint.LintResults` of one file, as `ToResult` writes its `result.json`: the checks'
/// warnings in the order they were found, the file, and the error that ended the
/// planning, if one did. The file carries no `definition`: BuildKit's is the frontend's
/// LLB for loading it, which names a session of that build alone (testdata/deviations.json).
#[derive(Debug)]
pub struct LintResults<'a> {
    pub warnings: &'a [crate::lint::Warning],
    pub filename: &'a [u8],
    pub data: &'a [u8],
    pub language: &'a [u8],
    pub error: Option<(&'a [u8], &'a Location)>,
}

impl LintResults<'_> {
    /// Its `result.json`.
    pub fn json(&self) -> String {
        let mut out = String::new();
        let mut o = Object::open(&mut out, 0);
        let w = o.key("warnings");
        if self.warnings.is_empty() {
            // A nil slice.
            w.push_str("null");
        } else {
            array(w, 1, self.warnings, |out, depth, warning| {
                let mut x = Object::open(out, depth);
                x.string("ruleName", warning.rule.as_bytes());
                for (k, v) in [
                    ("description", warning.description.as_bytes()),
                    ("url", warning.url.as_bytes()),
                    ("detail", &warning.message),
                ] {
                    if !v.is_empty() {
                        x.string(k, v);
                    }
                }
                let l = x.key("location");
                lint_location(l, depth + 1, &warning.location);
                x.close();
            });
        }
        let s = o.key("sources");
        s.push_str("[\n");
        indent(s, 2);
        let mut f = Object::open(s, 2);
        if !self.filename.is_empty() {
            f.string("filename", self.filename);
        }
        if !self.data.is_empty() {
            f.string("data", base64(self.data).as_bytes());
        }
        if !self.language.is_empty() {
            f.string("language", self.language);
        }
        f.close();
        s.push('\n');
        indent(s, 1);
        s.push(']');
        if let Some((message, loc)) = self.error {
            let e = o.key("buildError");
            let mut x = Object::open(e, 1);
            x.string("message", message);
            let l = x.key("location");
            lint_location(l, 2, loc);
            x.close();
        }
        o.close();
        out
    }
}

/// `pb.Location` as encoding/json writes it, its ranges left out when there are none.
fn lint_location(out: &mut String, depth: usize, loc: &Location) {
    if loc.is_empty() {
        out.push_str("{}");
    } else {
        location(out, depth, loc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_base64_as_encoding_json_writes_them() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    }
}
