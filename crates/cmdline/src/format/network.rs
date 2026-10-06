//! `network ls`'s rows (docker/cli formatter/network.go's networkContext, FormatWrite and
//! NewFormat).

use std::collections::BTreeMap;

use shards_template::Value;

use super::{Context, Ctx, Header, Methods, RAW, TABLE, join_labels};

/// A network as the API's network.Summary holds what the formatter reads of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub ipv4: bool,
    pub ipv6: bool,
    pub internal: bool,
    pub labels: BTreeMap<String, String>,
    /// When it was made, in nanoseconds since the epoch.
    pub created: i128,
}

const DEFAULT_TABLE: &str = "table {{.ID}}\t{{.Name}}\t{{.Driver}}\t{{.Scope}}";

/// NewFormat: the format `network ls` writes with, from `--format` (`table` when none)
/// and `--quiet`, which a template of the user's overrides.
pub fn format(source: &str, quiet: bool) -> String {
    match source {
        TABLE if quiet => "{{.ID}}".into(),
        TABLE => DEFAULT_TABLE.into(),
        RAW if quiet => "network_id: {{.ID}}".into(),
        RAW => "network_id: {{.ID}}\\nname: {{.Name}}\\ndriver: {{.Driver}}\\nscope: {{.Scope}}\\n".into(),
        _ => source.into(),
    }
}

/// FormatWrite: the networks, as `ctx.format` says.
pub fn write(ctx: &Context<'_>, networks: &[Network], out: &mut String) -> Result<(), String> {
    let rows = networks
        .iter()
        .map(|n| {
            Ctx::value(Row {
                network: n.clone(),
                trunc: ctx.trunc,
                created_at: ctx.clock.string(n.created),
            })
        })
        .collect();
    super::write(ctx, &HEADER, rows, out)
}

pub(super) static HEADER: Header = Header(&[
    ("CreatedAt", "CREATED AT"),
    ("Driver", "DRIVER"),
    ("ID", "NETWORK ID"),
    ("IPv4", "IPV4"),
    ("IPv6", "IPV6"),
    ("Internal", "INTERNAL"),
    ("Labels", "LABELS"),
    ("Name", "NAME"),
    ("Scope", "SCOPE"),
]);

/// networkContext.
#[derive(Debug)]
pub(super) struct Row {
    network: Network,
    trunc: bool,
    created_at: String,
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*network.networkContext"
    }
    const METHODS: &'static [&'static str] = &[
        "CreatedAt",
        "Driver",
        "ID",
        "IPv4",
        "IPv6",
        "Internal",
        "Labels",
        "Name",
        "Scope",
    ];
    const HEADER: &'static Header = &HEADER;
    const LABEL: bool = true;

    fn get(&self, name: &str) -> Option<Value> {
        let n = &self.network;
        let s = match name {
            "ID" if self.trunc => n.id.get(..12).unwrap_or(&n.id).to_string(),
            "ID" => n.id.clone(),
            "Name" => n.name.clone(),
            "Driver" => n.driver.clone(),
            "Scope" => n.scope.clone(),
            "IPv4" => n.ipv4.to_string(),
            "IPv6" => n.ipv6.to_string(),
            "Internal" => n.internal.to_string(),
            "Labels" => join_labels(&n.labels),
            "CreatedAt" => self.created_at.clone(),
            _ => return None,
        };
        Some(Value::String(s))
    }

    fn label(&self, name: &str) -> String {
        self.network.labels.get(name).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::clock::{Clock, utc};

    fn render(source: &str, quiet: bool, trunc: bool, networks: &[Network]) -> String {
        let clock = Clock { now: 0, zone: &utc };
        let ctx = Context {
            format: &format(source, quiet),
            trunc,
            east_asian: false,
            clock: &clock,
        };
        let mut out = String::new();
        write(&ctx, networks, &mut out).unwrap();
        out
    }

    /// What `docker network ls` (29.3.1) printed of the same networks.
    #[test]
    fn networks_are_listed_as_the_cli_lists_them() {
        let n = Network {
            id: "88552681c572c8e5054c20d6e316c923140de7e93785ff54fbe773950c8b9fe5".into(),
            name: "zzf1".into(),
            driver: "bridge".into(),
            scope: "local".into(),
            ipv4: true,
            labels: [("b", "two"), ("c", ""), ("zzl", "1")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            created: 1_791_260_013_875_605_088,
            ..Network::default()
        };
        assert_eq!(
            render(TABLE, false, true, std::slice::from_ref(&n)),
            "NETWORK ID     NAME      DRIVER    SCOPE\n88552681c572   zzf1      bridge    local\n"
        );
        assert_eq!(
            render(TABLE, false, true, &[]),
            "NETWORK ID   NAME      DRIVER    SCOPE\n"
        );
        assert_eq!(
            render(TABLE, true, true, std::slice::from_ref(&n)),
            "88552681c572\n"
        );
        assert_eq!(
            render(RAW, false, true, std::slice::from_ref(&n)),
            "network_id: 88552681c572\nname: zzf1\ndriver: bridge\nscope: local\n\n"
        );
        assert_eq!(
            render(
                "{{.Name}} {{.Labels}} {{.IPv4}} {{.Label \"zzl\"}}",
                true,
                true,
                std::slice::from_ref(&n)
            ),
            "zzf1 b=two,c=,zzl=1 true 1\n"
        );
        assert_eq!(
            render("{{json .}}", false, true, std::slice::from_ref(&n)),
            "{\"CreatedAt\":\"2026-10-06 04:13:33.875605088 +0000 UTC\",\"Driver\":\"bridge\",\"ID\":\"88552681c572\",\"IPv4\":\"true\",\"IPv6\":\"false\",\"Internal\":\"false\",\"Labels\":\"b=two,c=,zzl=1\",\"Name\":\"zzf1\",\"Scope\":\"local\"}\n"
        );
    }
}
