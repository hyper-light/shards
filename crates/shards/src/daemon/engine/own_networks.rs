//! The networks a microVM's own engine makes (D181), each a network of that microVM's
//! switch, which init holds: its clients create, list, inspect, remove and prune them, and
//! connect and disconnect its containers, in the calls Compose and Docker's SDKs make, and
//! are answered in the shapes they read with the switch's own values. Joining one grants a
//! member nothing; what each member declares is what its peers there reach (init's
//! `members.rs`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::Call;
use super::http::Answer;
use super::scope::Scope;
use crate::containers::{Disk, EngineNetwork};
use crate::daemon::Daemon;
use crate::daemon::lock;

/// The names of the networks every microVM's engine has of its own: its default, and
/// `host` and `none`, which are no network of its switch's.
const PREDEFINED: [&str; 3] = ["bridge", "host", "none"];

/// The bridge driver's options a network of a microVM's switch takes (Compose's and the
/// SDKs' names for them), and what each means there: its members' being one another's
/// peers, the address its members' published ports are bound at, and a gateway mode,
/// `isolated` being an internal network's.
const ICC: &str = "com.docker.network.bridge.enable_icc";
const HOST_BINDING: &str = "com.docker.network.bridge.host_binding_ipv4";
const GATEWAY_MODE4: &str = "com.docker.network.bridge.gateway_mode_ipv4";
/// The options no network of a microVM's switch can hold, each with why.
const UNHELD: [(&str, &str); 5] = [
    (
        "com.docker.network.bridge.enable_ip_masquerade",
        "its members' flows past the microVM leave by the microVM's own address alone",
    ),
    (
        "com.docker.network.bridge.name",
        "a network of a microVM's switch has no bridge to name",
    ),
    (
        "com.docker.network.bridge.inhibit_ipv4",
        "its members reach the switch by its gateway, which it keeps",
    ),
    (
        "com.docker.network.driver.mtu",
        "its members' links take the switch's MTU; another is not built yet",
    ),
    (
        "com.docker.network.container_iface_prefix",
        "a member's link on the switch is its one interface, eth0",
    ),
];

/// An ID of this microVM's engine's own for a network of its that no create made: `bridge`,
/// `host` or `none`, the same for as long as the microVM is.
pub(in crate::daemon) fn own_id(provider: &str, name: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(format!("{provider}\0{name}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl<D: Disk> Daemon<D> {
    /// What init answers a network builtin (D181) asked in the microVM of `scope`, as text,
    /// or why not.
    fn ask_switch(&self, scope: &Scope, argv: &[&str]) -> Result<String, String> {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::NETWORK,
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            ..Default::default()
        };
        let quiet = self
            .exec_quietly(&scope.provider, &spec, None, 1 << 12, crate::daemon::TAKE_TIMEOUT)
            .map_err(|e| format!("its microVM's switch: {e}"))?;
        let said = String::from_utf8_lossy(&quiet.output).trim().to_string();
        match quiet.status {
            Some(0) => Ok(said),
            _ => Err(said),
        }
    }

    /// The address a published port of `run`, a container this microVM's engine made, is
    /// bound at where its binding names none: the one the network it is published through,
    /// its first, names for its members' ports (`host_binding_ipv4`), if it names one.
    pub(in crate::daemon) fn engine_host_binding(&self, run: &shards_ipc::Run) -> Option<std::net::Ipv4Addr> {
        let (provider, _) = run.engine_made.as_ref()?;
        let first = run.engine_nets.first()?.split('\0').next()?;
        lock(&self.containers)
            .get(provider)?
            .engine_networks
            .iter()
            .find(|n| n.name == first)?
            .options
            .get(HOST_BINDING)?
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .filter(|a| !a.is_unspecified())
    }

    /// The networks microVM `scope`'s engine made.
    fn own_networks(&self, scope: &Scope) -> Vec<EngineNetwork> {
        lock(&self.containers)
            .get(&scope.provider)
            .map(|p| p.engine_networks.clone())
            .unwrap_or_default()
    }

    /// The members of network `name` of microVM `scope`'s engine, each a container it made
    /// that runs there.
    fn own_members(&self, scope: &Scope, name: &str) -> Vec<crate::daemon::networks::Member> {
        lock(&self.containers)
            .all()
            .filter_map(|c| {
                let made = c.engine_made.as_ref().filter(|m| m.of == scope.provider)?;
                let at = made.addresses.iter().find(|a| a.network == name)?;
                Some(crate::daemon::networks::Member {
                    container: c.id.clone(),
                    name: made.name.clone(),
                    endpoint: own_id(&c.id, name),
                    mac: shards_cmdline::network::mac_string(&at.mac),
                    ip: at.ip,
                    ip6: None,
                    dns_names: vec![made.name.clone()],
                    link_local: Vec::new(),
                })
            })
            .collect()
    }

    /// Network `n` of microVM `scope`'s engine, as the shape its clients read says it: one it
    /// made, or one of its own (its default, `host`, `none`).
    fn own_network_value(&self, scope: &Scope, name: &str) -> Option<Value> {
        let made = self.own_networks(scope).into_iter().find(|n| n.name == name);
        let created = lock(&self.containers)
            .get(&scope.provider)
            .map_or(0, |p| p.created);
        let network = match (made, name) {
            (Some(n), _) => crate::networks::Network {
                name: n.name.clone(),
                id: n.id.clone(),
                created: n.created,
                scope: "local".into(),
                driver: "bridge".into(),
                ipv4: true,
                ipv6: false,
                ipam_driver: "default".into(),
                ipam_options: BTreeMap::new(),
                pools: vec![crate::networks::Pool {
                    subnet: n.subnet,
                    ip_range: n.ip_range,
                    gateway: n.gateway,
                    aux: BTreeMap::new(),
                }],
                pools6: Vec::new(),
                v6_first: false,
                internal: n.internal,
                attachable: false,
                ingress: false,
                config_from: String::new(),
                config_only: false,
                options: n.options.clone(),
                labels: n.labels.clone(),
            },
            (None, "bridge" | "host" | "none") => {
                let pools = match name {
                    "bridge" => self
                        .ask_switch(scope, &["default"])
                        .ok()
                        .and_then(|said| made_pool(&said))
                        .map(|m| crate::networks::Pool::from(&m))
                        .into_iter()
                        .collect(),
                    _ => Vec::new(),
                };
                crate::networks::Network {
                    name: name.into(),
                    id: own_id(&scope.provider, name),
                    created,
                    scope: "local".into(),
                    driver: if name == "none" { "null" } else { name }.into(),
                    ipv4: name == "bridge",
                    ipv6: false,
                    ipam_driver: "default".into(),
                    ipam_options: BTreeMap::new(),
                    pools,
                    pools6: Vec::new(),
                    v6_first: false,
                    internal: false,
                    attachable: false,
                    ingress: false,
                    config_from: String::new(),
                    config_only: false,
                    // Its default names no peers: its members reach none of one another.
                    options: if name == "bridge" {
                        [(ICC.to_string(), "false".to_string())].into_iter().collect()
                    } else {
                        BTreeMap::new()
                    },
                    labels: BTreeMap::new(),
                }
            }
            _ => return None,
        };
        let members = self.own_members(scope, name);
        Some(
            serde_json::from_str(&crate::daemon::inspect_doc::json(
                &self.network_value_of(&network, &members),
            ))
            .unwrap_or_default(),
        )
    }

    /// The names of every network of microVM `scope`'s engine: its own, then those it made.
    fn own_network_names(&self, scope: &Scope) -> Vec<String> {
        PREDEFINED
            .iter()
            .map(|n| (*n).to_string())
            .chain(self.own_networks(scope).into_iter().map(|n| n.name))
            .collect()
    }

    /// The name of the network of microVM `scope`'s engine `term` names: by its name, its
    /// ID or the start of one ID alone.
    fn own_network_named(&self, scope: &Scope, term: &str) -> Option<String> {
        if PREDEFINED.contains(&term) {
            return Some(term.into());
        }
        if let Some(n) = PREDEFINED.iter().find(|n| own_id(&scope.provider, n) == term) {
            return Some((*n).to_string());
        }
        super::scope::engine_network(&self.own_networks(scope), term).map(|n| n.name.clone())
    }

    /// `GET /networks` of a microVM's own engine: its networks, each as the list sums one up
    /// (its inspect document, less who is on it), by the filters its client gives: `name`,
    /// `id`, `label` and `driver` as given, `type` builtin (its own) or custom (made).
    pub(super) fn scoped_networks(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let filters = match super::containers::filters(call.query("filters")) {
            Ok(f) => f,
            Err(e) => return Answer::error(400, e),
        };
        let wanted = |key: &str| -> Vec<String> {
            filters
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .collect()
        };
        for (key, _) in &filters {
            if !matches!(
                key.as_str(),
                "name" | "id" | "label" | "driver" | "type" | "scope"
            ) {
                return Answer::error(400, format!("invalid filter '{key}'"));
            }
        }
        let mut listed = Vec::new();
        for name in self.own_network_names(scope) {
            let Some(mut n) = self.own_network_value(scope, &name) else {
                continue;
            };
            let text = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let (id, driver) = (text("Id"), text("Driver"));
            let builtin = PREDEFINED.contains(&name.as_str());
            let keep = |key: &str, ok: &dyn Fn(&str) -> bool| {
                let w = wanted(key);
                w.is_empty() || w.iter().any(|v| ok(v))
            };
            let labels = n.get("Labels").cloned().unwrap_or_default();
            let kept = keep("name", &|v| name.contains(v))
                && keep("id", &|v| id.starts_with(v))
                && keep("driver", &|v| driver == v)
                && keep("scope", &|v| v == "local")
                && keep("type", &|v| (v == "builtin") == builtin)
                && keep("label", &|v| match v.split_once('=') {
                    Some((k, val)) => labels.get(k).and_then(Value::as_str) == Some(val),
                    None => labels.get(v).is_some(),
                });
            if kept {
                if let Some(o) = n.as_object_mut() {
                    for inspect_only in ["Containers", "Services", "Status"] {
                        o.remove(inspect_only);
                    }
                }
                listed.push(n);
            }
        }
        Answer::json(200, &Value::Array(listed))
    }

    /// `GET /networks/{term}` of a microVM's own engine.
    pub(super) fn scoped_network(&self, term: &str, scope: &Scope) -> Answer {
        match self
            .own_network_named(scope, term)
            .and_then(|name| self.own_network_value(scope, &name))
        {
            Some(n) => Answer::json(200, &n),
            None => Answer::error(404, format!("No such network: {term}")),
        }
    }

    /// `POST /networks/create` of a microVM's own engine: a network of its microVM's switch,
    /// its subnet init's to give or check, as its client asks for it; what the switch
    /// cannot hold refused with why.
    pub(super) fn scoped_network_create(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a network's body: {e}")),
        };
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let flag = |k: &str| body.get(k).and_then(Value::as_bool).unwrap_or(false);
        let name = text(&body, "Name");
        if name.trim().is_empty() {
            return Answer::error(400, "a network is made by its name (Name)");
        }
        if PREDEFINED.contains(&name.as_str()) || name == "default" {
            return Answer::error(
                403,
                format!(
                    "network {name}: one of its engine's own, bridge, host and none: give yours another name"
                ),
            );
        }
        if self.own_networks(scope).iter().any(|n| n.name == name) {
            return Answer::error(409, format!("a network is named {name} already"));
        }
        let refused = |why: String| Answer::error(400, format!("network {name}: {why}"));
        match text(&body, "Driver").as_str() {
            "" | "bridge" => {}
            d => {
                return refused(format!(
                    "driver {d}: a network of a microVM's own engine is a network of that microVM's switch, made as a bridge network is asked for"
                ));
            }
        }
        if body.get("EnableIPv6").and_then(Value::as_bool) == Some(true) {
            return refused("IPv6 between a microVM's engine's containers is not built yet".into());
        }
        if body.get("EnableIPv4").and_then(Value::as_bool) == Some(false) {
            return refused("a network of a microVM's switch has IPv4, which it keeps".into());
        }
        for (key, what) in [
            (
                "Ingress",
                "an ingress network is a swarm's, which no microVM's engine is",
            ),
            ("ConfigOnly", "a network of configuration alone is not built yet"),
        ] {
            if flag(key) {
                return refused(what.into());
            }
        }
        if body
            .get("ConfigFrom")
            .map(|c| text(c, "Network"))
            .is_some_and(|n| !n.is_empty())
        {
            return refused("a network made from another's configuration is not built yet".into());
        }
        if !matches!(text(&body, "Scope").as_str(), "" | "local") {
            return refused(format!(
                "scope {}: a network of a microVM's switch is its microVM's alone",
                text(&body, "Scope")
            ));
        }
        let ipam = body.get("IPAM").unwrap_or(&Value::Null);
        if !matches!(text(ipam, "Driver").as_str(), "" | "default") {
            return refused(format!(
                "IPAM driver {}: init gives a switch network's addresses itself",
                text(ipam, "Driver")
            ));
        }
        let pools: Vec<&Value> = ipam
            .get("Config")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
        let (v6, v4): (Vec<&Value>, Vec<&Value>) =
            pools.iter().partition(|p| text(p, "Subnet").contains(':'));
        if !v6.is_empty() {
            return refused("IPv6 between a microVM's engine's containers is not built yet".into());
        }
        if v4.len() > 1 {
            return refused("a network of a microVM's switch has one IPv4 subnet".into());
        }
        let pool = v4.first().copied().unwrap_or(&Value::Null);
        if pool
            .get("AuxiliaryAddresses")
            .and_then(Value::as_object)
            .is_some_and(|a| !a.is_empty())
        {
            return refused(
                "auxiliary addresses on a network of a microVM's switch are not built yet".into(),
            );
        }
        let options: BTreeMap<String, String> = body
            .get("Options")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some((key, why)) = UNHELD.iter().find(|(k, _)| options.contains_key(*k)) {
            return refused(format!("{key}: {why}"));
        }
        let peers = match options.get(ICC).map(String::as_str) {
            None | Some("true" | "1") => true,
            Some("false" | "0") => false,
            Some(v) => return refused(format!("{ICC} {v}: true or false")),
        };
        let mut internal = flag("Internal");
        match options.get(GATEWAY_MODE4).map(String::as_str) {
            None | Some("nat") => {}
            Some("isolated") => internal = true,
            Some(m) => {
                return refused(format!(
                    "{GATEWAY_MODE4} {m}: a member reaches past its microVM by the microVM's own address, or not at all (isolated)"
                ));
            }
        }
        if let Some(v) = options.get(HOST_BINDING)
            && v.parse::<std::net::Ipv4Addr>().is_err()
        {
            return refused(format!("{HOST_BINDING} {v}: no IPv4 address"));
        }
        let Ok(id) = crate::containers::new_id() else {
            return Answer::error(500, "making the network's ID");
        };
        let flags: Vec<&str> = [(internal, "internal"), (!peers, "no-peers")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, f)| *f)
            .collect();
        let (subnet, gateway, range) = (text(pool, "Subnet"), text(pool, "Gateway"), text(pool, "IPRange"));
        let said = match self.ask_switch(
            scope,
            &["make", &name, &subnet, &gateway, &range, &flags.join(",")],
        ) {
            Ok(said) => said,
            Err(why) => return refused(why),
        };
        let Some(mut made) = made_pool(&said) else {
            return Answer::error(500, format!("network {name}: its microVM's switch said {said:?}"));
        };
        made.ip_range = range
            .split_once('/')
            .and_then(|(a, p)| Some((a.parse().ok()?, p.parse().ok()?)));
        let labels: BTreeMap<String, String> = body
            .get("Labels")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let network = EngineNetwork {
            id: id.clone(),
            name: name.clone(),
            created: crate::containers::now(),
            subnet: made.subnet,
            gateway: made.gateway,
            ip_range: made.ip_range,
            internal,
            peers,
            labels,
            options,
        };
        let kept =
            lock(&self.containers).change(&scope.provider, |p| p.engine_networks.push(network.clone()));
        if let Err(e) = kept {
            let _ = self.ask_switch(scope, &["remove", &name]);
            return Answer::error(500, format!("network {name}: {e}"));
        }
        self.record_soon(&scope.provider, Vec::new());
        Answer::json(201, &json!({ "Id": id, "Warning": "" }))
    }

    /// `DELETE /networks/{term}` of a microVM's own engine: one it made, none of whose
    /// members runs, gone from its switch.
    pub(super) fn scoped_network_remove(&self, term: &str, scope: &Scope) -> Answer {
        let Some(name) = self.own_network_named(scope, term) else {
            return Answer::error(404, format!("No such network: {term}"));
        };
        if PREDEFINED.contains(&name.as_str()) {
            return Answer::error(
                403,
                format!("network {name}: one of its engine's own, which goes with its microVM alone"),
            );
        }
        match self.remove_own_network(scope, &name) {
            Ok(()) => Answer::text(204, ""),
            Err((status, why)) => Answer::error(status, why),
        }
    }

    /// Removes network `name` of microVM `scope`'s engine, none of whose members runs.
    fn remove_own_network(&self, scope: &Scope, name: &str) -> Result<(), (u16, String)> {
        let running: Vec<String> = self
            .own_members(scope, name)
            .into_iter()
            .map(|m| m.name)
            .collect();
        if !running.is_empty() {
            return Err((
                403,
                format!("network {name}: its members run on it: {}", running.join(", ")),
            ));
        }
        self.ask_switch(scope, &["remove", name])
            .map_err(|why| (500, format!("network {name}: {why}")))?;
        lock(&self.containers)
            .change(&scope.provider, |p| p.engine_networks.retain(|n| n.name != name))
            .map_err(|e| (500, format!("network {name}: {e}")))?;
        self.record_soon(&scope.provider, Vec::new());
        Ok(())
    }

    /// `POST /networks/prune` of a microVM's own engine: each network it made that no member
    /// runs on, and the filters keep (`label`, `until`), removed; their names.
    pub(super) fn scoped_network_prune(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let filters = match super::containers::filters(call.query("filters")) {
            Ok(f) => f,
            Err(e) => return Answer::error(400, e),
        };
        let now = i128::try_from(crate::containers::now()).unwrap_or(i128::MAX);
        let mut until: Option<i128> = None;
        for (key, value) in &filters {
            match key.as_str() {
                "label" | "label!" => {}
                "until" => {
                    match shards_cmdline::gotime::parse_timestamp(
                        value,
                        now,
                        i64::from(crate::cli::utc_offset()),
                    ) {
                        Ok(t) => until = Some(t),
                        Err(e) => return Answer::error(400, e),
                    }
                }
                other => return Answer::error(400, format!("invalid filter '{other}'")),
            }
        }
        let mut removed = Vec::new();
        for n in self.own_networks(scope) {
            if until.is_some_and(|t| i128::try_from(n.created).unwrap_or(i128::MAX) > t) {
                continue;
            }
            let labelled = filters.iter().all(|(key, v)| {
                let has = match v.split_once('=') {
                    Some((k, val)) => n.labels.get(k).is_some_and(|x| x == val),
                    None => n.labels.contains_key(v),
                };
                match key.as_str() {
                    "label" => has,
                    "label!" => !has,
                    _ => true,
                }
            });
            if labelled && self.remove_own_network(scope, &n.name).is_ok() {
                removed.push(n.name);
            }
        }
        Answer::json(200, &json!({ "NetworksDeleted": removed }))
    }

    /// `POST /networks/{term}/connect` and `disconnect` of a microVM's own engine: a
    /// container of its that does not run joins or leaves the network for its next start;
    /// one that runs, not yet.
    pub(super) fn scoped_network_join(
        &self,
        term: &str,
        call: &Call<'_>,
        scope: &Scope,
        join: bool,
    ) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a network's body: {e}")),
        };
        let Some(name) = self.own_network_named(scope, term) else {
            return Answer::error(404, format!("No such network: {term}"));
        };
        if matches!(name.as_str(), "host" | "none") {
            return Answer::error(
                403,
                format!("network {name}: no container joins or leaves it but as it is made"),
            );
        }
        let given = body.get("Container").and_then(Value::as_str).unwrap_or_default();
        self.settle();
        let scoped = self.scoped(scope);
        let Some(c) = super::scope::resolve(&scoped, given) else {
            return Answer::error(404, format!("No such container: {given}"));
        };
        let id = c.id.clone();
        let scoped_name = c.name.clone();
        let endpoint = body.get("EndpointConfig").unwrap_or(&Value::Null);
        let aliases: Vec<String> = endpoint
            .get("Aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let addr = endpoint
            .get("IPAMConfig")
            .and_then(|i| i.get("IPv4Address"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if name == "bridge" && !aliases.is_empty() {
            return Answer::error(
                400,
                "network bridge: its engine's default network names no peers, so no alias of a member's is resolved there",
            );
        }
        // One that runs is connected or disconnected at once, by init, as its member's
        // links are made; its next start keeps it (below).
        if self.running(&id) {
            let guest = self.inbox_of(&id).and_then(|inbox| lock(&inbox).guest);
            let Some(guest) = guest else {
                return Answer::error(409, format!("{scoped_name} is not running"));
            };
            let guest = guest.to_string();
            let mut argv: Vec<&str> = vec![if join { "connect" } else { "disconnect" }, &guest, &name];
            if join {
                argv.push(&addr);
                argv.push(&scoped_name);
                argv.extend(aliases.iter().map(String::as_str));
            }
            let said = match self.ask_switch(scope, &argv) {
                Ok(said) => said,
                Err(why) => return Answer::error(400, format!("network {name}: {scoped_name}: {why}")),
            };
            let mac = lock(&self.containers)
                .get(&id)
                .and_then(|c| c.engine_made.as_ref())
                .and_then(|m| m.addresses.first().map(|a| a.mac));
            let address = if join {
                let mac = mac
                    .map(|m| shards_cmdline::network::mac_string(&m))
                    .unwrap_or_default();
                match crate::daemon::engine_address_of(format!("{name} {said} {mac}").as_bytes()) {
                    Some(a) => Some(a),
                    None => {
                        return Answer::error(
                            500,
                            format!("network {name}: {scoped_name}: its microVM's switch said {said:?}"),
                        );
                    }
                }
            } else {
                None
            };
            let changed = lock(&self.containers).change(&id, |c| {
                if let Some(made) = c.engine_made.as_mut() {
                    made.addresses.retain(|a| a.network != name);
                    made.addresses.extend(address.clone());
                }
            });
            if let Err(e) = changed {
                return Answer::error(500, format!("network {name}: {scoped_name}: {e}"));
            }
            self.record_soon(&id, Vec::new());
        }
        let changed = self.change_request(&id, |run| {
            let at = run
                .engine_nets
                .iter()
                .position(|e| e.split('\0').next() == Some(name.as_str()));
            match (join, at) {
                (true, Some(_)) => Err((403, format!("{} is on network {name} already", c.name))),
                (true, None) => {
                    let mut entry = format!("{name}\0{addr}");
                    for a in &aliases {
                        entry.push('\0');
                        entry.push_str(a);
                    }
                    run.engine_nets.push(entry);
                    if run.engine_net.is_empty() || run.engine_net == "none" {
                        run.engine_net.clone_from(&name);
                    }
                    Ok(())
                }
                (false, None) => Err((404, format!("{} is on no network {name}", c.name))),
                (false, Some(at)) => {
                    run.engine_nets.remove(at);
                    run.engine_net = run
                        .engine_nets
                        .first()
                        .and_then(|e| e.split('\0').next())
                        .unwrap_or("none")
                        .to_string();
                    Ok(())
                }
            }
        });
        match changed {
            Ok(()) => Answer::text(200, ""),
            Err((status, why)) => Answer::error(status, why),
        }
    }
}

impl<D: Disk> Daemon<D> {
    /// Changes the request container `id` was made with, as `change` says, and keeps it: what
    /// its next start reads.
    fn change_request(
        &self,
        id: &str,
        change: impl FnOnce(&mut shards_ipc::Run) -> Result<(), (u16, String)>,
    ) -> Result<(), (u16, String)> {
        let dir = lock(&self.containers).dir(id);
        let path = dir.join(crate::daemon::REQUEST);
        let mut run = std::fs::read(&path)
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .ok_or_else(|| (500, format!("container {id}: its request is not kept")))?;
        change(&mut run)?;
        std::fs::write(&path, run.encode()).map_err(|e| (500, format!("container {id}: {e}")))
    }
}

/// A network init made, as it says it (`SUBNET GATEWAY`): its pool.
struct Made {
    subnet: (std::net::Ipv4Addr, u8),
    gateway: std::net::Ipv4Addr,
    ip_range: Option<(std::net::Ipv4Addr, u8)>,
}

fn made_pool(said: &str) -> Option<Made> {
    let (subnet, gateway) = said.trim().split_once(' ')?;
    let (addr, prefix) = subnet.split_once('/')?;
    Some(Made {
        subnet: (addr.parse().ok()?, prefix.parse().ok()?),
        gateway: gateway.parse().ok()?,
        ip_range: None,
    })
}

impl From<&Made> for crate::networks::Pool {
    fn from(m: &Made) -> Self {
        crate::networks::Pool {
            subnet: m.subnet,
            ip_range: m.ip_range,
            gateway: m.gateway,
            aux: BTreeMap::new(),
        }
    }
}
