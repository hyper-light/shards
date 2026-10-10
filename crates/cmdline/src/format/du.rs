//! `buildx du`'s records (buildx v0.37.1 commands/diskusage.go): runDiskUsage's formats
//! and summary and its diskusageContext, through docker/cli's formatter as buildx vendors
//! it, the records sorted as BuildKit's client sorts them (client/diskusage.go). Held to
//! buildx's own output by tests/buildx_du.rs (scripts/buildx/du_test.go).

use shards_template::{Kind, Value};

use super::units::human_size;
use super::{Clock, Context, Ctx, Header, Methods, context_format, is_json, is_table, parse, post_format};

/// A build cache record as BuildKit's DiskUsage answers it (client.UsageInfo).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub id: String,
    pub parents: Vec<String>,
    /// Nanoseconds since the epoch.
    pub created_at: i128,
    pub mutable: bool,
    pub in_use: bool,
    pub shared: bool,
    pub size: i64,
    pub description: String,
    pub usage_count: i64,
    /// Nanoseconds since the epoch; none where it was never used.
    pub last_used_at: Option<i128>,
    /// RecordType.
    pub kind: String,
}

/// duDefaultTableFormat.
pub const TABLE_FORMAT: &str = "table {{.ID}}\t{{.Reclaimable}}\t{{.Size}}\t{{.LastUsedAt}}";

/// duDefaultPrettyTemplate.
pub const PRETTY: &str = "ID:           {{.ID}}
{{- if .Parents }}
Parents:
{{- range .Parents }}
 - {{.}}
{{- end }}
{{- end }}
Created at:   {{.CreatedAt}}
Mutable:      {{.Mutable}}
Reclaimable:  {{.Reclaimable}}
Shared:       {{.Shared}}
Size:         {{.Size}}
{{- if .Description}}
Description:  {{ .Description }}
{{- end }}
Usage count:  {{.UsageCount}}
{{- if .LastUsedAt}}
Last used:    {{ .LastUsedAt }}
{{- end }}
{{- if .Type}}
Type:         {{ .Type }}
{{- end }}
";

/// runDiskUsage's format from `--format` and `--verbose`: its table, `--verbose` and
/// `pretty` its template, `table` its table; both given an error.
pub fn format(given: &str, verbose: bool) -> Result<String, String> {
    if !given.is_empty() && verbose {
        return Err("--format and --verbose cannot be used together".into());
    }
    Ok(match given {
        "" if verbose => PRETTY,
        "" | super::TABLE => TABLE_FORMAT,
        "pretty" => PRETTY,
        other => other,
    }
    .into())
}

/// runDiskUsage's output once BuildKit has answered: the records, sorted by size then ID,
/// each through `ctx.format` (made by [`format`]), then printSummary's totals where the
/// format is du's own, not JSON, and no filter was `filtered`. On an error nothing is
/// written but the totals, as the deferred printSummary still prints them.
pub fn write(ctx: &Context<'_>, records: &[Usage], filtered: bool, out: &mut String) -> Result<(), String> {
    let mut records = records.to_vec();
    records.sort_by(|a, b| a.size.cmp(&b.size).then_with(|| a.id.cmp(&b.id)));
    let written = lay(ctx, &records, out);
    let own = ctx.format == TABLE_FORMAT || ctx.format == PRETTY;
    if own && !is_json(ctx.format) && !filtered {
        summary(&records, out);
    }
    written
}

/// formatter.Context.Write over the records' contexts.
fn lay(ctx: &Context<'_>, records: &[Usage], out: &mut String) -> Result<(), String> {
    let tmpl = parse(ctx.format)?;
    let table = is_table(ctx.format);
    let mut buffer = String::new();
    for r in records {
        let row = Ctx::value(Row {
            r: r.clone(),
            table,
            ago: r.last_used_at.map(|t| ctx.clock.ago(t)),
        });
        context_format(&tmpl, &row, &mut buffer)?;
    }
    post_format(ctx.format, ctx.east_asian, &tmpl, &HEADER, &buffer, out);
    Ok(())
}

/// printSummary: shared and private where any is shared, then what could be reclaimed
/// and the total, through a tabwriter of minimum width 1, tab width 8 and padding 1 that
/// pads with tabs.
#[allow(clippy::cast_precision_loss)]
fn summary(records: &[Usage], out: &mut String) {
    let (mut total, mut reclaimable, mut shared) = (0i64, 0i64, 0i64);
    for r in records {
        if r.size > 0 {
            total = total.wrapping_add(r.size);
            if !r.in_use {
                reclaimable = reclaimable.wrapping_add(r.size);
            }
        }
        if r.shared {
            shared = shared.wrapping_add(r.size);
        }
    }
    let mut lines = Vec::new();
    if shared > 0 {
        lines.push(("Shared:", human_size(shared as f64)));
        lines.push(("Private:", human_size(total.wrapping_sub(shared) as f64)));
    }
    lines.push(("Reclaimable:", human_size(reclaimable as f64)));
    lines.push(("Total:", human_size(total as f64)));
    let width = lines
        .iter()
        .map(|(k, _)| k.len() + 1)
        .max()
        .unwrap_or(0)
        .div_ceil(8)
        * 8;
    for (k, v) in lines {
        out.push_str(k);
        out.push_str(&"\t".repeat((width - k.len()).div_ceil(8)));
        out.push_str(&v);
        out.push('\n');
    }
}

static HEADER: Header = Header(&[
    ("CreatedAt", "CREATED AT"),
    ("Description", "DESCRIPTION"),
    ("ID", "ID"),
    ("LastUsedAt", "LAST ACCESSED"),
    ("Mutable", "MUTABLE"),
    ("Parents", "PARENTS"),
    ("Reclaimable", "RECLAIMABLE"),
    ("Shared", "SHARED"),
    ("Size", "SIZE"),
    ("Type", "TYPE"),
    ("UsageCount", "USAGE COUNT"),
]);

/// diskusageContext: a record, whether the format is a table, and how long ago it was
/// last used, worked out from the clock.
#[derive(Debug)]
struct Row {
    r: Usage,
    table: bool,
    ago: Option<String>,
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*commands.diskusageContext"
    }
    const METHODS: &'static [&'static str] = &[
        "CreatedAt",
        "Description",
        "ID",
        "LastUsedAt",
        "Mutable",
        "Parents",
        "Reclaimable",
        "Shared",
        "Size",
        "Type",
        "UsageCount",
    ];
    const HEADER: &'static Header = &HEADER;

    #[allow(clippy::cast_precision_loss)]
    fn get(&self, name: &str) -> Option<Value> {
        let r = &self.r;
        Some(match name {
            // In UTC, as BuildKit's client makes its times.
            "CreatedAt" => Value::String(
                Clock {
                    now: 0,
                    zone: &super::utc,
                }
                .string(r.created_at),
            ),
            "Description" => Value::String(r.description.clone()),
            "ID" if self.table && r.mutable => Value::String(format!("{}*", r.id)),
            "ID" => Value::String(r.id.clone()),
            "LastUsedAt" => Value::String(self.ago.clone().unwrap_or_default()),
            "Mutable" => Value::Bool(r.mutable),
            // A record with none has Go's nil slice.
            "Parents" if r.parents.is_empty() => Value::NilList(Kind::String),
            "Parents" => Value::strings(r.parents.iter().cloned()),
            "Reclaimable" => Value::Bool(!r.in_use),
            "Shared" => Value::Bool(r.shared),
            "Size" if self.table && r.shared => Value::String(format!("{}*", human_size(r.size as f64))),
            "Size" => Value::String(human_size(r.size as f64)),
            "Type" => Value::String(r.kind.clone()),
            "UsageCount" => Value::Int(r.usage_count),
            _ => return None,
        })
    }
}
