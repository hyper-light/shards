//! Networks through the API (D170): `shards network`'s networks of microVMs (D46).
//! Sandboxes that work together reach one another by name; each network is closed to
//! all but its members, who reach one another past the default deny and nothing else,
//! their names answered at the resolver address programs written for Docker ask.

use serde_json::{Value, json};

use super::http::Answer;
use super::{Call, refused};
use crate::containers::Disk;
use crate::daemon::{Daemon, Threads};

/// An object of strings as `KEY=VALUE`s.
fn pairs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default()
}

impl<D: Disk> Daemon<D> {
    /// `GET /networks`: `shards network ls`, by the query's filters, each network as the
    /// API sums one up (its inspect document, less who is on it).
    pub(super) fn engine_networks(&self, call: &Call<'_>) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        match self.networks_listed(&given) {
            Ok(networks) => {
                let summaries: Vec<Value> = networks
                    .iter()
                    .filter_map(|n| serde_json::from_str::<Value>(&crate::daemon::inspect_doc::json(n)).ok())
                    .map(|mut n| {
                        if let Some(o) = n.as_object_mut() {
                            for inspect_only in ["Containers", "Services", "Status"] {
                                o.remove(inspect_only);
                            }
                        }
                        n
                    })
                    .collect();
                Answer::json(200, &Value::Array(summaries))
            }
            Err(e) => refused(&e, 400),
        }
    }

    /// `GET /networks/{id}`: `shards network inspect`.
    pub(super) fn engine_network(&self, term: &str) -> Answer {
        match self.network_doc(term) {
            Ok(doc) => Answer::document(200, crate::daemon::inspect_doc::json(&doc)),
            Err(e) => refused(&e, 400),
        }
    }

    /// `POST /networks/create`: `shards network create`. A network asked for without
    /// `Internal` is told how every one is: closed to all but its members.
    pub(super) fn engine_network_create<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        call: &Call<'_>,
    ) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a network's body: {e}")),
        };
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let flag = |k: &str| body.get(k).and_then(Value::as_bool).unwrap_or(false);
        let name = text(&body, "Name");
        if name.is_empty() {
            return Answer::error(400, "a network is created by its name (Name)");
        }
        let mut argv: Vec<String> = vec!["network".into(), "create".into()];
        for (key, option) in [("Driver", "--driver"), ("Scope", "--scope")] {
            if !text(&body, key).is_empty() {
                argv.extend([option.into(), text(&body, key)]);
            }
        }
        for (key, option) in [
            ("Internal", "--internal"),
            ("Attachable", "--attachable"),
            ("Ingress", "--ingress"),
            ("ConfigOnly", "--config-only"),
        ] {
            if flag(key) {
                argv.push(option.into());
            }
        }
        if let Some(from) = body
            .get("ConfigFrom")
            .map(|c| text(c, "Network"))
            .filter(|n| !n.is_empty())
        {
            argv.extend(["--config-from".into(), from]);
        }
        if body.get("EnableIPv4").and_then(Value::as_bool) == Some(false) {
            argv.push("--ipv4=false".into());
        }
        if flag("EnableIPv6") {
            argv.push("--ipv6".into());
        }
        let ipam = body.get("IPAM").unwrap_or(&Value::Null);
        if !text(ipam, "Driver").is_empty() {
            argv.extend(["--ipam-driver".into(), text(ipam, "Driver")]);
        }
        for o in pairs(ipam.get("Options")) {
            argv.extend(["--ipam-opt".into(), o]);
        }
        for pool in ipam.get("Config").and_then(Value::as_array).into_iter().flatten() {
            for (key, option) in [
                ("Subnet", "--subnet"),
                ("IPRange", "--ip-range"),
                ("Gateway", "--gateway"),
            ] {
                if !text(pool, key).is_empty() {
                    argv.extend([option.into(), text(pool, key)]);
                }
            }
            for aux in pairs(pool.get("AuxiliaryAddresses")) {
                argv.extend(["--aux-address".into(), aux]);
            }
        }
        for (key, option) in [("Options", "--opt"), ("Labels", "--label")] {
            for pair in pairs(body.get(key)) {
                argv.extend([option.into(), pair]);
            }
        }
        argv.push(name);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.ask_command(threads, &argv) {
            Ok((0, out, _)) => {
                let id = String::from_utf8_lossy(&out).trim().to_string();
                let warning = if flag("Internal") {
                    ""
                } else {
                    "a shards network is closed to all but its members, who reach one another and nothing else; what else a microVM reaches, its own grants say"
                };
                Answer::json(201, &json!({ "Id": id, "Warning": warning }))
            }
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `DELETE /networks/{id}`: `shards network rm`.
    pub(super) fn engine_network_remove<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        term: &str,
    ) -> Answer {
        match self.ask_command(threads, &["network", "rm", term]) {
            Ok((0, _, _)) => Answer::text(204, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /networks/{id}/connect` and `disconnect`: `shards network connect` and
    /// `disconnect`. A created or stopped microVM's endpoint, kept for its next start; a
    /// running one keeps the one network device it started with (D46), and that is said.
    pub(super) fn engine_network_join<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        term: &str,
        call: &Call<'_>,
        join: bool,
    ) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a network's body: {e}")),
        };
        let container = body
            .get("Container")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if container.is_empty() {
            return Answer::error(
                400,
                "a microVM joins or leaves a network by its name or ID (Container)",
            );
        }
        let mut argv: Vec<String> = vec![
            "network".into(),
            (if join { "connect" } else { "disconnect" }).into(),
        ];
        if join {
            let endpoint = body.get("EndpointConfig").unwrap_or(&Value::Null);
            if endpoint
                .get("Links")
                .and_then(Value::as_array)
                .is_some_and(|l| !l.is_empty())
            {
                return Answer::error(
                    400,
                    "EndpointConfig.Links: a microVM reaches another by its network's names, as each grant to it allows; legacy links grant what no directive names",
                );
            }
            if endpoint
                .get("MacAddress")
                .and_then(Value::as_str)
                .is_some_and(|m| !m.is_empty())
            {
                return Answer::error(
                    400,
                    "EndpointConfig.MacAddress: a connect sets no MAC yet, as shards' CLI has none to set, and the API reaches nothing its CLI does not; give it as the microVM is made",
                );
            }
            let ipam = endpoint.get("IPAMConfig").unwrap_or(&Value::Null);
            for (key, option) in [("IPv4Address", "--ip"), ("IPv6Address", "--ip6")] {
                if let Some(a) = ipam.get(key).and_then(Value::as_str).filter(|a| !a.is_empty()) {
                    argv.extend([option.into(), a.to_string()]);
                }
            }
            for a in ipam
                .get("LinkLocalIPs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                argv.extend([
                    "--link-local-ip".into(),
                    a.as_str().unwrap_or_default().to_string(),
                ]);
            }
            for a in endpoint
                .get("Aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                argv.extend(["--alias".into(), a.as_str().unwrap_or_default().to_string()]);
            }
            for o in pairs(endpoint.get("DriverOpts")) {
                argv.extend(["--driver-opt".into(), o]);
            }
            if let Some(p) = endpoint
                .get("GwPriority")
                .and_then(Value::as_i64)
                .filter(|p| *p != 0)
            {
                argv.extend(["--gw-priority".into(), p.to_string()]);
            }
        } else if body.get("Force").and_then(Value::as_bool).unwrap_or(false) {
            argv.push("--force".into());
        }
        argv.extend([term.to_string(), container]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.ask_command(threads, &argv) {
            Ok((0, _, _)) => Answer::text(200, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /networks/prune`: `shards network prune`. The user networks no microVM is on
    /// that the filters keep, removed: their names.
    pub(super) fn engine_network_prune(&self, call: &Call<'_>) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        let now = i128::try_from(crate::containers::now()).unwrap_or(i128::MAX);
        match self.prune_networks(&given, now, i64::from(crate::cli::utc_offset())) {
            Ok(names) => Answer::json(200, &json!({ "NetworksDeleted": names })),
            Err(e) => refused(&e, 400),
        }
    }
}
