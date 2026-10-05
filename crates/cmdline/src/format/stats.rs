//! `stats`' rows (container/formatter_stats.go's statsContext, statsFormatWrite and
//! NewStatsFormat), for Linux guests: Windows' columns never apply to shards' microVMs.

use shards_template::Value;

use super::units::{bytes_size, human_size_precision};
use super::{Context, Ctx, Header, Methods, TABLE, truncate_id};

/// A container's sample (StatsEntry).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// The name or ID it was asked for by.
    pub container: String,
    pub name: String,
    pub id: String,
    pub cpu_percentage: f64,
    pub memory: f64,
    pub memory_limit: f64,
    pub memory_percentage: f64,
    pub network_rx: f64,
    pub network_tx: f64,
    pub block_read: f64,
    pub block_write: f64,
    pub pids: u64,
    /// No sample could be taken.
    pub invalid: bool,
}

const DEFAULT_TABLE: &str = "table {{.ID}}\t{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}\t{{.NetIO}}\t{{.BlockIO}}\t{{.PIDs}}";
const NO_VALUE: &str = "--";

/// NewStatsFormat: the format `stats` writes with, from `--format` (`table` when none).
pub fn format(source: &str) -> String {
    if source == TABLE {
        return DEFAULT_TABLE.into();
    }
    source.into()
}

/// statsFormatWrite: the samples, as `ctx.format` says; IDs truncated with `ctx.trunc`.
pub fn write(ctx: &Context<'_>, stats: &[Stats], out: &mut String) -> Result<(), String> {
    let rows = stats
        .iter()
        .map(|s| {
            Ctx::value(Row {
                s: s.clone(),
                trunc: ctx.trunc,
            })
        })
        .collect();
    super::write(ctx, &HEADER, rows, out)
}

static HEADER: Header = Header(&[
    ("BlockIO", "BLOCK I/O"),
    ("CPUPerc", "CPU %"),
    ("Container", "CONTAINER"),
    ("ID", "CONTAINER ID"),
    ("MemPerc", "MEM %"),
    ("MemUsage", "MEM USAGE / LIMIT"),
    ("Name", "NAME"),
    ("NetIO", "NET I/O"),
    ("PIDs", "PIDS"),
]);

/// statsContext.
#[derive(Debug)]
struct Row {
    s: Stats,
    trunc: bool,
}

/// formatPercentage: strconv.FormatFloat(v, 'f', 2, 64) and `%`.
fn percentage(v: f64) -> String {
    if v.is_nan() {
        "NaN%".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf%" } else { "-Inf%" }.into()
    } else {
        format!("{v:.2}%")
    }
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*container.statsContext"
    }
    const METHODS: &'static [&'static str] = &[
        "BlockIO",
        "CPUPerc",
        "Container",
        "ID",
        "MemPerc",
        "MemUsage",
        "Name",
        "NetIO",
        "PIDs",
    ];
    const HEADER: &'static Header = &HEADER;

    fn get(&self, name: &str) -> Option<Value> {
        let s = &self.s;
        let pair =
            |a: f64, b: f64| format!("{} / {}", human_size_precision(a, 3), human_size_precision(b, 3));
        let v = match name {
            "BlockIO" if s.invalid => NO_VALUE.into(),
            "BlockIO" => pair(s.block_read, s.block_write),
            "CPUPerc" if s.invalid => NO_VALUE.into(),
            "CPUPerc" => percentage(s.cpu_percentage),
            "Container" => s.container.clone(),
            "ID" if self.trunc => truncate_id(&s.id),
            "ID" => s.id.clone(),
            "MemPerc" if s.invalid => NO_VALUE.into(),
            "MemPerc" => percentage(s.memory_percentage),
            "MemUsage" if s.invalid => "-- / --".into(),
            "MemUsage" => format!("{} / {}", bytes_size(s.memory), bytes_size(s.memory_limit)),
            "Name" => match s.name.strip_prefix('/').unwrap_or(&s.name) {
                "" => NO_VALUE.into(),
                n => n.into(),
            },
            "NetIO" if s.invalid => NO_VALUE.into(),
            "NetIO" => pair(s.network_rx, s.network_tx),
            "PIDs" if s.invalid => NO_VALUE.into(),
            "PIDs" => s.pids.to_string(),
            _ => return None,
        };
        Some(Value::String(v))
    }
}
