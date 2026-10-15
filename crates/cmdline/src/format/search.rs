//! `search`'s rows (docker/cli cli/command/registry/formatter_search.go: searchContext,
//! formatWrite and newFormat), and shards' own of each result (D172): whether it runs on
//! this machine, on which platform, what its pull takes, and its pulls. Those a template
//! may name (`{{.RunsHere}}`, `{{.Platform}}`, `{{.Platforms}}`, `{{.Size}}`,
//! `{{.PullCount}}`); `{{json .}}` says what the Docker CLI's says.

use std::cell::Cell;

use shards_template::Value;

use super::units::human_size_precision;
use super::{Context, Ctx, Header, Methods, TABLE};
use crate::width::ellipsis;

/// A result as the API's registry.SearchResult holds what the formatter reads of it, and
/// what shards found of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub description: String,
    pub star_count: i64,
    pub is_official: bool,
    /// Whether it runs here, where that was looked up.
    pub here: Option<Here>,
    /// How many times it was pulled, where its registry counts it.
    pub pulls: Option<i64>,
}

/// Whether a result runs on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Here {
    /// On this platform, its pull this many bytes.
    Runs { platform: String, size: u64 },
    /// Its images are for these platforms alone.
    Elsewhere(Vec<String>),
    /// It has no `latest`, which a run that names no tag pulls.
    Untagged,
    /// Its lookup failed, for this reason.
    Unknown(String),
}

const DEFAULT_TABLE: &str = "table {{.Name}}\t{{.Description}}\t{{.StarCount}}\t{{.IsOfficial}}";

/// newFormat: the format `search` writes with, from `--format` (`table` when none).
pub fn format(source: &str) -> String {
    match source {
        "" | TABLE => DEFAULT_TABLE.into(),
        _ => source.into(),
    }
}

/// formatWrite: the results, as `ctx.format` says.
pub fn write(ctx: &Context<'_>, results: &[Found], out: &mut String) -> Result<(), String> {
    let rows = results
        .iter()
        .map(|f| {
            Ctx::value(Row {
                found: f.clone(),
                trunc: ctx.trunc,
                json: Cell::new(false),
            })
        })
        .collect();
    super::write(ctx, &HEADER, rows, out)
}

pub(super) static HEADER: Header = Header(&[
    ("Description", "DESCRIPTION"),
    ("IsOfficial", "OFFICIAL"),
    ("Name", "NAME"),
    ("StarCount", "STARS"),
]);

/// searchContext. `json` is its field of that name, which its MarshalJSON sets for good:
/// a template reads `IsOfficial` as JSON says it from then on.
#[derive(Debug)]
pub(super) struct Row {
    found: Found,
    trunc: bool,
    json: Cell<bool>,
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*registry.searchContext"
    }
    const METHODS: &'static [&'static str] = &["Description", "IsOfficial", "Name", "StarCount"];
    const EXTRA: &'static [&'static str] = &["Platform", "Platforms", "PullCount", "RunsHere", "Size"];
    const HEADER: &'static Header = &HEADER;

    fn marshaling(&self) {
        self.json.set(true);
    }

    fn get(&self, name: &str) -> Option<Value> {
        let f = &self.found;
        let s = match name {
            "Name" => f.name.clone(),
            "Description" => {
                let one_line = f.description.replace(['\n', '\r'], " ");
                if self.trunc {
                    ellipsis(&one_line, 45)
                } else {
                    one_line
                }
            }
            "StarCount" => f.star_count.to_string(),
            // formatBool.
            "IsOfficial" => match (f.is_official, self.json.get()) {
                (true, true) => "true".into(),
                (true, false) => "[OK]".into(),
                (false, true) => "false".into(),
                (false, false) => String::new(),
            },
            // Unknown where it was not looked up, or could not be.
            "RunsHere" => match &f.here {
                Some(Here::Runs { .. }) => "true".into(),
                Some(Here::Elsewhere(_) | Here::Untagged) => "false".into(),
                Some(Here::Unknown(_)) | None => String::new(),
            },
            "Platform" => match &f.here {
                Some(Here::Runs { platform, .. }) => platform.clone(),
                _ => String::new(),
            },
            "Platforms" => match &f.here {
                Some(Here::Runs { platform, .. }) => platform.clone(),
                Some(Here::Elsewhere(platforms)) => platforms.join(","),
                _ => String::new(),
            },
            // As `images` says an image's size (go-units' HumanSizeWithPrecision).
            #[allow(clippy::cast_precision_loss)]
            "Size" => match &f.here {
                Some(Here::Runs { size, .. }) => human_size_precision(*size as f64, 3),
                _ => String::new(),
            },
            "PullCount" => f.pulls.map(|n| n.to_string()).unwrap_or_default(),
            _ => return None,
        };
        Some(Value::String(s))
    }
}
