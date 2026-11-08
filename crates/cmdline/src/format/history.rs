//! `history`'s rows (image/formatter_history.go's historyContext, historyWrite and
//! newHistoryFormat).

use shards_template::Value;

use super::units::human_size_precision;
use super::{Context, Ctx, DEFAULT_QUIET, Header, Methods, TABLE, truncate_id};
use crate::width::ellipsis;

/// A layer of an image's history (image.HistoryResponseItem).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// `<missing>` for a layer pulled with the image.
    pub id: String,
    /// Seconds since the epoch.
    pub created: i64,
    pub created_by: String,
    pub size: i64,
    pub comment: String,
}

const DEFAULT_TABLE: &str = "table {{.ID}}\t{{.CreatedSince}}\t{{.CreatedBy}}\t{{.Size}}\t{{.Comment}}";
const NON_HUMAN_TABLE: &str = "table {{.ID}}\t{{.CreatedAt}}\t{{.CreatedBy}}\t{{.Size}}\t{{.Comment}}";

/// newHistoryFormat: the format `history` writes with, from `--format` (`table` when
/// none), `--quiet` and `--human`.
pub fn format(source: &str, quiet: bool, human: bool) -> String {
    match source {
        TABLE if quiet => DEFAULT_QUIET.into(),
        TABLE if !human => NON_HUMAN_TABLE.into(),
        TABLE => DEFAULT_TABLE.into(),
        _ => source.into(),
    }
}

/// historyWrite: the layers, as `ctx.format` says; sizes and times in words with `human`.
pub fn write(ctx: &Context<'_>, human: bool, layers: &[History], out: &mut String) -> Result<(), String> {
    let rows = layers
        .iter()
        .map(|h| {
            let created_at = ctx.clock.rfc3339(h.created);
            // Before 2000 a date is not given in words (formatter_history.go's epoch).
            let created_since = if !human {
                created_at.clone()
            } else if h.created <= 946_684_800 {
                "N/A".into()
            } else {
                ctx.clock.ago(i128::from(h.created) * 1_000_000_000)
            };
            Ctx::value(Row {
                h: h.clone(),
                trunc: ctx.trunc,
                human,
                created_at,
                created_since,
            })
        })
        .collect();
    super::write(ctx, &HEADER, rows, out)
}

static HEADER: Header = Header(&[
    ("Comment", "COMMENT"),
    ("CreatedAt", "CREATED AT"),
    ("CreatedBy", "CREATED BY"),
    ("CreatedSince", "CREATED"),
    ("ID", "IMAGE"),
    ("Size", "SIZE"),
]);

/// historyContext, with its times worked out from the clock.
#[derive(Debug)]
struct Row {
    h: History,
    trunc: bool,
    human: bool,
    created_at: String,
    created_since: String,
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*image.historyContext"
    }
    const METHODS: &'static [&'static str] =
        &["Comment", "CreatedAt", "CreatedBy", "CreatedSince", "ID", "Size"];
    const HEADER: &'static Header = &HEADER;

    fn get(&self, name: &str) -> Option<Value> {
        let h = &self.h;
        let s = match name {
            "Comment" => h.comment.clone(),
            "CreatedAt" => self.created_at.clone(),
            "CreatedBy" => {
                let by = h.created_by.replace('\t', " ");
                if self.trunc { ellipsis(&by, 45) } else { by }
            }
            "CreatedSince" => self.created_since.clone(),
            "ID" if self.trunc => truncate_id(&h.id),
            "ID" => h.id.clone(),
            #[allow(clippy::cast_precision_loss)]
            "Size" if self.human => human_size_precision(h.size as f64, 3),
            "Size" => h.size.to_string(),
            _ => return None,
        };
        Some(Value::String(s))
    }
}
