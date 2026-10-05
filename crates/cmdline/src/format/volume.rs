//! `volume ls`'s rows (formatter/volume.go's volumeContext, VolumeWrite and
//! NewVolumeFormat).

use std::collections::BTreeMap;

use shards_template::Value;

use super::units::human_size;
use super::{Context, Ctx, Header, Methods, RAW, TABLE, join_labels};

/// A volume as the API's volume.Volume holds what the formatter reads of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub mountpoint: String,
    pub labels: BTreeMap<String, String>,
    /// UsageData's reference count and size, where the daemon worked them out (`system
    /// df`).
    pub usage: Option<(i64, i64)>,
}

const DEFAULT_TABLE: &str = "table {{.Driver}}\t{{.Name}}";
const DEFAULT_QUIET: &str = "{{.Name}}";

/// NewVolumeFormat: the format `volume ls` writes with, from `--format` (`table` when
/// none) and `--quiet`.
pub fn format(source: &str, quiet: bool) -> String {
    match source {
        TABLE if quiet => DEFAULT_QUIET.into(),
        TABLE => DEFAULT_TABLE.into(),
        RAW if quiet => "name: {{.Name}}".into(),
        RAW => "name: {{.Name}}\\ndriver: {{.Driver}}\\n".into(),
        _ => source.into(),
    }
}

/// VolumeWrite: the volumes, as `ctx.format` says.
pub fn write(ctx: &Context<'_>, volumes: &[Volume], out: &mut String) -> Result<(), String> {
    let rows = volumes.iter().map(|v| Ctx::value(Row(v.clone()))).collect();
    super::write(ctx, &HEADER, rows, out)
}

pub(super) static HEADER: Header = Header(&[
    ("Availability", "AVAILABILITY"),
    ("Driver", "DRIVER"),
    ("Group", "GROUP"),
    ("ID", "ID"),
    ("Labels", "LABELS"),
    ("Links", "LINKS"),
    ("Mountpoint", "MOUNTPOINT"),
    ("Name", "VOLUME NAME"),
    ("Scope", "SCOPE"),
    ("Size", "SIZE"),
    ("Status", "STATUS"),
]);

/// volumeContext.
#[derive(Debug)]
pub(super) struct Row(pub(super) Volume);

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*formatter.volumeContext"
    }
    const METHODS: &'static [&'static str] = &[
        "Availability",
        "Driver",
        "Group",
        "Labels",
        "Links",
        "Mountpoint",
        "Name",
        "Scope",
        "Size",
        "Status",
    ];
    const HEADER: &'static Header = &HEADER;
    const LABEL: bool = true;

    fn get(&self, name: &str) -> Option<Value> {
        let v = &self.0;
        let s = match name {
            "Name" => v.name.clone(),
            "Driver" => v.driver.clone(),
            "Scope" => v.scope.clone(),
            "Mountpoint" => v.mountpoint.clone(),
            // Sorted, as the CLI sorts them; none for a volume with none.
            "Labels" => join_labels(&v.labels),
            "Links" => v.usage.map_or("N/A".into(), |(refs, _)| refs.to_string()),
            #[allow(clippy::cast_precision_loss)]
            "Size" => v.usage.map_or("N/A".into(), |(_, size)| human_size(size as f64)),
            // A cluster volume's, which a volume of no swarm's is not.
            "Group" | "Availability" | "Status" => "N/A".into(),
            _ => return None,
        };
        Some(Value::String(s))
    }

    fn label(&self, name: &str) -> String {
        self.0.labels.get(name).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::clock::{Clock, utc};

    fn render(source: &str, quiet: bool, volumes: &[Volume]) -> String {
        let clock = Clock { now: 0, zone: &utc };
        let ctx = Context {
            format: &format(source, quiet),
            trunc: true,
            east_asian: false,
            clock: &clock,
        };
        let mut out = String::new();
        write(&ctx, volumes, &mut out).unwrap();
        out
    }

    /// What `docker volume ls` (29.3.1) printed of the same volumes, sorted by name as
    /// the CLI sorts them before writing.
    #[test]
    fn volumes_are_listed_as_the_cli_lists_them() {
        let local = |name: &str, labels: &[(&str, &str)]| Volume {
            name: name.into(),
            driver: "local".into(),
            scope: "local".into(),
            mountpoint: format!("/var/lib/docker/volumes/{name}/_data"),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            usage: None,
        };
        let vols = [
            local("foobar_bar", &[("a", "1"), ("b", "")]),
            local("foobar_baz", &[]),
        ];
        assert_eq!(
            render(TABLE, false, &vols),
            "DRIVER    VOLUME NAME\nlocal     foobar_bar\nlocal     foobar_baz\n"
        );
        assert_eq!(render(TABLE, true, &vols), "foobar_bar\nfoobar_baz\n");
        assert_eq!(
            render(RAW, false, &vols),
            "name: foobar_bar\ndriver: local\n\nname: foobar_baz\ndriver: local\n\n"
        );
        assert_eq!(
            render(
                "table {{.Name}}\t{{.Labels}}\t{{.Label \"a\"}}\t{{.Links}}",
                false,
                &vols
            ),
            "VOLUME NAME   LABELS    a         LINKS\nfoobar_bar    a=1,b=    1         N/A\nfoobar_baz                        N/A\n"
        );
        assert_eq!(
            render("{{json .}}", false, &vols[1..]),
            "{\"Availability\":\"N/A\",\"Driver\":\"local\",\"Group\":\"N/A\",\"Labels\":\"\",\"Links\":\"N/A\",\"Mountpoint\":\"/var/lib/docker/volumes/foobar_baz/_data\",\"Name\":\"foobar_baz\",\"Scope\":\"local\",\"Size\":\"N/A\",\"Status\":\"N/A\"}\n"
        );
    }
}
