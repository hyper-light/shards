//! `ps`'s rows (formatter/container.go's ContainerContext, ContainerWrite and
//! DisplayablePorts; container/list.go's checks of a `--format`).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::rc::Rc;

use shards_template::{Template, Value};

use super::reference::parse_normalized_named;
use super::units::human_size_precision;
use super::{Clock, Context, Ctx, DEFAULT_QUIET, Header, Methods, RAW, TABLE, join_labels, truncate_id};
use crate::go::quote;
use crate::width::ellipsis;

/// A container as `ps` lists it: the fields of moby's container.Summary the CLI prints.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Container {
    pub id: String,
    /// With their leading `/`, as dockerd lists them.
    pub names: Vec<String>,
    pub image: String,
    pub image_id: String,
    /// The platform of the image manifest it runs, where known.
    pub platform: Option<Platform>,
    pub command: String,
    /// Seconds since the epoch.
    pub created: i64,
    pub ports: Vec<Port>,
    pub size_rw: i64,
    pub size_root_fs: i64,
    pub labels: BTreeMap<String, String>,
    /// `running`, `exited`, …
    pub state: String,
    /// `Up 2 hours (healthy)`, …
    pub status: String,
    /// Health.Status, where it has a health check: `healthy`, `starting`, …
    pub health: String,
    /// The networks it is attached to.
    pub networks: Vec<String>,
    pub mounts: Vec<Mount>,
}

/// A container's port (container.PortSummary): the address published on (None for an
/// exposed port), its private and public ports, and its protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub ip: Option<IpAddr>,
    pub private: u16,
    pub public: u16,
    pub kind: String,
}

/// A container's mount (container.MountPoint): its volume's name, else its source, and
/// its volume's driver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mount {
    pub name: String,
    pub source: String,
    pub driver: String,
}

/// An OCI platform (ocispec.Platform): its OS, architecture and variant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: String,
}

const DEFAULT_TABLE: &str =
    "table {{.ID}}\t{{.Image}}\t{{.Command}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}\t{{.Names}}";

/// NewContainerFormat: the format `ps` writes with, from `--format` (`table` when none),
/// `--quiet` and whether sizes are listed.
pub fn format(source: &str, quiet: bool, size: bool) -> String {
    match source {
        TABLE | "" => {
            if quiet {
                return DEFAULT_QUIET.into();
            }
            let mut f = DEFAULT_TABLE.to_string();
            if size {
                f.push_str("\\t{{.Size}}");
            }
            f
        }
        RAW => {
            if quiet {
                return "container_id: {{.ID}}".into();
            }
            let mut f = String::from(
                "container_id: {{.ID}}\nimage: {{.Image}}\ncommand: {{.Command}}\ncreated_at: {{.CreatedAt}}\n\
                 state: {{- pad .State 1 0}}\nstatus: {{- pad .Status 1 0}}\nnames: {{.Names}}\n\
                 labels: {{- pad .Labels 1 0}}\nports: {{- pad .Ports 1 0}}\n",
            );
            if size {
                f.push_str("size: {{.Size}}\\n");
            }
            f
        }
        _ if quiet => DEFAULT_QUIET.into(),
        _ => source.into(),
    }
}

/// container/list.go's buildContainerListOptions, of a `--format` given: parsed, and run
/// against an empty container, as the CLI checks it before listing; whether it shows
/// `.Size`, which makes `ps` list sizes unless `--size` or `--quiet` was given.
pub fn check(format: &str, clock: &Clock<'_>) -> Result<bool, String> {
    let tmpl = Template::parse("", format).map_err(|e| format!("failed to parse template: {e}"))?;
    let used = Rc::new(Cell::new(false));
    let row = Row::new(&Container::default(), false, clock, Rc::clone(&used));
    let ctx = Value::Object(Rc::new(Ctx { m: row, header: true }));
    let mut discard = String::new();
    tmpl.execute_into(&ctx, &mut discard)
        .map_err(|e| format!("failed to execute template: {e}"))?;
    Ok(used.get())
}

/// ContainerWrite: the containers, as `ctx.format` says.
pub fn write(ctx: &Context<'_>, containers: &[Container], out: &mut String) -> Result<(), String> {
    let rows = rows(containers, ctx.trunc, ctx.clock);
    super::write(ctx, &HEADER, rows, out)
}

pub(super) fn rows(containers: &[Container], trunc: bool, clock: &Clock<'_>) -> Vec<Value> {
    containers
        .iter()
        .map(|c| Ctx::value(Row::new(c, trunc, clock, Rc::new(Cell::new(false)))))
        .collect()
}

pub(super) static HEADER: Header = Header(&[
    ("Command", "COMMAND"),
    ("CreatedAt", "CREATED AT"),
    ("HealthStatus", "HEALTH STATUS"),
    ("ID", "CONTAINER ID"),
    ("Image", "IMAGE"),
    ("Labels", "LABELS"),
    ("LocalVolumes", "LOCAL VOLUMES"),
    ("Mounts", "MOUNTS"),
    ("Names", "NAMES"),
    ("Networks", "NETWORKS"),
    ("Platform", "PLATFORM"),
    ("Ports", "PORTS"),
    ("RunningFor", "CREATED"),
    ("Size", "SIZE"),
    ("State", "STATE"),
    ("Status", "STATUS"),
]);

/// ContainerContext, with its times worked out from the clock.
#[derive(Debug)]
pub(super) struct Row {
    c: Container,
    trunc: bool,
    created_at: String,
    running_for: String,
    /// Set when `.Size` is called (ContainerContext.FieldsUsed).
    size_used: Rc<Cell<bool>>,
}

impl Row {
    fn new(c: &Container, trunc: bool, clock: &Clock<'_>, size_used: Rc<Cell<bool>>) -> Row {
        let created = i128::from(c.created) * 1_000_000_000;
        Row {
            c: c.clone(),
            trunc,
            created_at: clock.string(created),
            running_for: clock.ago(created),
            size_used,
        }
    }

    /// ContainerContext.Names: without their `/`; truncated, the first that is not a
    /// legacy link.
    fn names(&self) -> String {
        let mut all = Vec::new();
        for n in &self.c.names {
            let name = n.strip_prefix('/').unwrap_or(n);
            if self.trunc {
                if !name.contains('/') {
                    return name.into();
                }
                continue;
            }
            all.push(name);
        }
        all.join(",")
    }

    /// ContainerContext.Image: truncated, an ID as the short ID, and a reference in its
    /// familiar form without its digest.
    fn image(&self) -> String {
        let image = &self.c.image;
        if image.is_empty() {
            return "<no image>".into();
        }
        if !self.trunc {
            return image.clone();
        }
        let short = truncate_id(&self.c.image_id);
        if short == truncate_id(image) {
            return short;
        }
        match parse_normalized_named(image) {
            Some(mut r) => {
                r.digest.clear();
                r.familiar_string()
            }
            None => image.clone(),
        }
    }

    /// ContainerContext.Size.
    fn size(&self) -> String {
        self.size_used.set(true);
        #[allow(clippy::cast_precision_loss)]
        let rw = human_size_precision(self.c.size_rw as f64, 3);
        if self.c.size_root_fs > 0 {
            #[allow(clippy::cast_precision_loss)]
            let v = human_size_precision(self.c.size_root_fs as f64, 3);
            return format!("{rw} (virtual {v})");
        }
        rw
    }

    fn mounts(&self) -> String {
        let names: Vec<String> = self
            .c
            .mounts
            .iter()
            .map(|m| {
                let name = if m.name.is_empty() { &m.source } else { &m.name };
                if self.trunc {
                    ellipsis(name, 15)
                } else {
                    name.clone()
                }
            })
            .collect();
        names.join(",")
    }

    /// ContainerContext.HealthStatus: the health dockerd gives, else the one in the status.
    fn health(&self) -> String {
        if !self.c.health.is_empty() {
            return self.c.health.clone();
        }
        let Some((_, health)) = self.c.status.split_once('(') else {
            return String::new();
        };
        let Some(health) = health.strip_suffix(')') else {
            return String::new();
        };
        let health = health.strip_prefix("health: ").unwrap_or(health);
        match health {
            "healthy" | "unhealthy" | "starting" => health.into(),
            _ => String::new(),
        }
    }
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*formatter.ContainerContext"
    }
    const METHODS: &'static [&'static str] = &[
        "Command",
        "CreatedAt",
        "HealthStatus",
        "ID",
        "Image",
        "Labels",
        "LocalVolumes",
        "Mounts",
        "Names",
        "Networks",
        "Platform",
        "Ports",
        "RunningFor",
        "Size",
        "State",
        "Status",
    ];
    const HEADER: &'static Header = &HEADER;
    const LABEL: bool = true;

    fn get(&self, name: &str) -> Option<Value> {
        let c = &self.c;
        let s = match name {
            "Command" => quote(&if self.trunc {
                ellipsis(&c.command, 20)
            } else {
                c.command.clone()
            }),
            "CreatedAt" => self.created_at.clone(),
            "HealthStatus" => self.health(),
            "ID" => {
                if self.trunc {
                    truncate_id(&c.id)
                } else {
                    c.id.clone()
                }
            }
            "Image" => self.image(),
            "Labels" => join_labels(&c.labels),
            "LocalVolumes" => c
                .mounts
                .iter()
                .filter(|m| m.driver == "local")
                .count()
                .to_string(),
            "Mounts" => self.mounts(),
            "Names" => self.names(),
            "Networks" => {
                let mut n = c.networks.clone();
                n.sort();
                n.join(",")
            }
            "Platform" => return Some(Value::object(PlatformValue(c.platform.clone()))),
            "Ports" => displayable_ports(&c.ports),
            "RunningFor" => self.running_for.clone(),
            "Size" => self.size(),
            "State" => c.state.clone(),
            "Status" => c.status.clone(),
            _ => return None,
        };
        Some(Value::String(s))
    }

    fn label(&self, name: &str) -> String {
        self.c.labels.get(name).cloned().unwrap_or_default()
    }
}

/// ContainerContext.Platform's `*formatter.Platform`: printed as platforms.FormatAll
/// prints it, `<nil>` for none.
#[derive(Debug)]
struct PlatformValue(Option<Platform>);

impl shards_template::Object for PlatformValue {
    /// Without the pointer's `*`: fmt calls its String, so prints no `&`.
    fn type_name(&self) -> &str {
        "formatter.Platform"
    }

    fn field(&self, name: &str) -> Option<Value> {
        let p = self.0.as_ref()?;
        let s = match name {
            "OS" => &p.os,
            "Architecture" => &p.architecture,
            "Variant" => &p.variant,
            "OSVersion" => return Some(Value::String(String::new())),
            _ => return None,
        };
        Some(Value::String(s.clone()))
    }

    fn method(&self, name: &str) -> Option<&'static [shards_template::Kind]> {
        (name == "String").then_some(&[])
    }

    fn call(&self, name: &str, _: &[Value]) -> Result<Value, String> {
        let mut s = String::new();
        self.format(&mut s);
        match name {
            "String" => Ok(Value::String(s)),
            _ => Err(format!("no method {name}")),
        }
    }

    /// platforms.FormatAll: `unknown` without an OS, else the parts there are joined by
    /// slashes.
    fn format(&self, out: &mut String) {
        let Some(p) = &self.0 else {
            out.push_str("<nil>");
            return;
        };
        if p.os.is_empty() {
            out.push_str("unknown");
            return;
        }
        let parts: Vec<&str> = [&p.os, &p.architecture, &p.variant]
            .into_iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        out.push_str(&parts.join("/"));
    }

    /// The embedded ocispec.Platform's fields.
    fn json(&self, out: &mut String) -> Result<(), String> {
        let Some(p) = &self.0 else {
            out.push_str("null");
            return Ok(());
        };
        out.push_str("{\"architecture\":");
        super::json_string(&p.architecture, out);
        out.push_str(",\"os\":");
        super::json_string(&p.os, out);
        if !p.variant.is_empty() {
            out.push_str(",\"variant\":");
            super::json_string(&p.variant, out);
        }
        out.push('}');
        Ok(())
    }
}

/// DisplayablePorts: ports sorted, runs of consecutive ports of one address and protocol
/// as ranges, and the ports published elsewhere than their own number after them all
/// (`0.0.0.0:80->9090/tcp, 9988/tcp`).
pub fn displayable_ports(ports: &[Port]) -> String {
    let mut ports = ports.to_vec();
    ports.sort_by(|a, b| {
        a.private
            .cmp(&b.private)
            .then_with(|| a.ip.cmp(&b.ip))
            .then_with(|| a.public.cmp(&b.public))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    // Each key's current run, in the order keys were first seen.
    let mut groups: Vec<(String, u16, u16)> = Vec::new();
    let mut result = Vec::new();
    let mut host_mappings = Vec::new();
    for port in &ports {
        let current = port.private;
        let mut key = port.kind.clone();
        if let Some(ip) = port.ip {
            if port.public != current {
                host_mappings.push(format!(
                    "{}->{}/{}",
                    join_host_port(&ip.to_string(), &port.public.to_string()),
                    port.private,
                    port.kind
                ));
                continue;
            }
            key = format!("{ip}/{}", port.kind);
        }
        match groups.iter_mut().find(|(k, _, _)| *k == key) {
            None => groups.push((key, current, current)),
            Some(group) => {
                if current == group.2.wrapping_add(1) {
                    group.2 = current;
                    continue;
                }
                result.push(form_group(&group.0, group.1, group.2));
                group.1 = current;
                group.2 = current;
            }
        }
    }
    for (key, first, last) in &groups {
        result.push(form_group(key, *first, *last));
    }
    result.extend(host_mappings);
    result.join(", ")
}

/// formGroup.
fn form_group(key: &str, start: u16, last: u16) -> String {
    let parts: Vec<&str> = key.split('/').collect();
    let (ip, kind) = match parts.as_slice() {
        [ip, kind, ..] => (*ip, *kind),
        [kind] => ("", *kind),
        [] => ("", ""),
    };
    let mut group = start.to_string();
    if start != last {
        group = format!("{group}-{last}");
    }
    if !ip.is_empty() {
        group = format!("{}->{group}", join_host_port(ip, &group));
    }
    format!("{group}/{kind}")
}

/// net.JoinHostPort: an address with a colon in brackets.
fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
