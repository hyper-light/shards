//! A microVM's own engine (`--use-api-socket`, D181): the Engine API a run's command
//! reaches from inside its microVM, on connections its VM hands the daemon, scoped to that
//! microVM. Through it the command sees and acts on the containers its engine made alone,
//! each a joiner of its microVM (D119), by the names it gave them; what would reach past
//! the microVM is refused, with why.

use serde_json::{Value, json};
use shards_cmdline::flags::{self, Outcome};
use shards_ipc::Run;

use super::http::Answer;
use super::{Call, Done, containers::filters, create, plain, refused, status_of};
use crate::containers::{Container, Disk, State as Life};
use crate::daemon::{Daemon, Threads, lock};

/// The microVM a connection's calls are scoped to: its run's container ID.
pub(in crate::daemon) struct Scope {
    pub(in crate::daemon) provider: String,
}

/// Whether a microVM's own engine serves `method` on `path`: one it does not is answered
/// before its body is read.
pub(super) fn serves(method: &str, path: &[&str]) -> bool {
    matches!(
        (method, path),
        ("GET" | "HEAD", ["_ping"])
            | ("GET", ["version" | "info"])
            | ("POST", ["containers", "create"])
            | ("GET", ["containers", "json"])
            | ("GET", ["containers", _, "json" | "logs"])
            | (
                "POST",
                [
                    "containers",
                    _,
                    "start" | "stop" | "kill" | "wait" | "attach" | "resize" | "exec"
                ]
            )
            | ("DELETE", ["containers", _])
            | ("POST", ["exec", _, "start" | "resize"])
            | ("GET", ["exec", _, "json"])
            | ("POST", ["images", "create"])
            | ("GET", ["images", .., "json"])
            | ("GET", ["networks"] | ["networks", _])
            | ("POST", ["networks", "create" | "prune"])
            | ("DELETE", ["networks", _])
            | ("POST", ["networks", _, "connect" | "disconnect"])
            | ("GET", ["volumes"] | ["volumes", _])
            | ("POST", ["volumes", "create" | "prune"])
            | ("DELETE", ["volumes", _])
    )
}

/// What a microVM's own engine answers a call it does not serve.
pub(super) fn unserved(method: &str, target: &str) -> Answer {
    if method == "POST" && target.trim_end_matches('/').ends_with("/auth") {
        return Answer::error(
            403,
            "a registry login from inside a microVM: its registries' credentials stay with the run's client, outside it",
        );
    }
    Answer::error(
        501,
        format!(
            "{method} {target}: not served by a microVM's own engine, which serves its _ping, version and info, \
             its own containers' create, list, inspect, start, stop, kill, wait, logs, attach, exec \
             and remove, its images' pull, list and inspect, its networks' create, list, inspect, \
             remove, prune, connect and disconnect, and its volumes' create, list, inspect, \
             remove and prune"
        ),
    )
}

/// A container a scope cannot see, said as dockerd says one that is not there.
fn no_such(given: &str) -> Answer {
    Answer::error(404, format!("No such container: {given}"))
}

/// A bind of `point`'s, a path of its microVM's own command's files (D181), as init takes
/// one (`SRC\0DST\0FLAGS`: `ro` or `rw`, `create` for a source `-v` makes where it is
/// missing, as dockerd makes a host's): none for a mount that is no bind; why not for a
/// propagation that would reach the command's own mounts, which a copy of them in the
/// container's namespace shares nothing with.
fn own_bind(point: &crate::volumes::MountPoint) -> Option<Result<String, String>> {
    if point.kind != "bind" {
        return None;
    }
    if !matches!(point.propagation.as_str(), "" | "private" | "rprivate") {
        return Some(Err(format!(
            "propagation {}: a container's copy of its microVM's own files shares no mount with the command's",
            point.propagation
        )));
    }
    let mut flags = vec![if point.rw { "rw" } else { "ro" }];
    if point.create_source {
        flags.push("create");
    }
    Some(Ok(format!(
        "{}\0{}\0{}",
        point.source,
        point.destination,
        flags.join(",")
    )))
}

/// The network of `networks`, a microVM's own engine's (D181), that `term` names: by its
/// name, its ID, or the start of one ID alone.
pub(super) fn engine_network<'n>(
    networks: &'n [crate::containers::EngineNetwork],
    term: &str,
) -> Option<&'n crate::containers::EngineNetwork> {
    if let Some(n) = networks.iter().find(|n| n.name == term || n.id == term) {
        return Some(n);
    }
    let mut prefixed = networks
        .iter()
        .filter(|n| !term.is_empty() && n.id.starts_with(term));
    match (prefixed.next(), prefixed.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// The images microVM `provider`'s own engine reaches: its own, and those it pulled.
fn images_of(provider: &Container) -> Vec<String> {
    provider
        .image_id
        .iter()
        .chain(&provider.engine_images)
        .cloned()
        .collect()
}

/// The containers of `all` that the engine of microVM `provider` made, each by the name
/// it has there.
fn of_scope<'a>(all: impl Iterator<Item = &'a Container>, provider: &str) -> Vec<Container> {
    all.filter_map(|c| {
        let made = c.engine_made.as_ref().filter(|m| m.of == provider)?;
        let mut seen = c.clone();
        seen.name.clone_from(&made.name);
        Some(seen)
    })
    .collect()
}

/// `given`, as a scope's client names a container, among `scoped`, as dockerd resolves a
/// reference: its whole ID, its name, then a prefix of its ID that no other has.
pub(super) fn resolve<'a>(scoped: &'a [Container], given: &str) -> Option<&'a Container> {
    let given = given.trim();
    let name = given.strip_prefix('/').unwrap_or(given);
    if given.is_empty() {
        return None;
    }
    if let Some(c) = scoped.iter().find(|c| c.id == given) {
        return Some(c);
    }
    if let Some(c) = scoped.iter().find(|c| c.name == name) {
        return Some(c);
    }
    let mut prefixed = scoped.iter().filter(|c| c.id.starts_with(given));
    match (prefixed.next(), prefixed.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// A network of its engine's a create asks a container onto: the address its client asked
/// for there (none for one init gives), its aliases there, and its priority for the
/// default route.
struct Membership {
    network: String,
    addr: String,
    aliases: Vec<String>,
    priority: i64,
}

impl Membership {
    fn of(network: String) -> Membership {
        Membership {
            network,
            addr: String::new(),
            aliases: Vec::new(),
            priority: 0,
        }
    }
}

/// What of a create's run would reach past its microVM, refused with why; what names a
/// container is taken in the scope (`scoped`), as its engine's clients name them.
fn confine(
    run: &mut Run,
    provider: &str,
    scoped: &[Container],
    networks: &[crate::containers::EngineNetwork],
) -> Result<(), Answer> {
    let past = |what: String| {
        Err(Answer::error(
            403,
            format!(
                "{what}: a container a microVM's own engine makes is that microVM's, and takes nothing of its host's"
            ),
        ))
    };
    // It joins its microVM (D119) as a member of that microVM's switch: a network
    // namespace of its own there, on each network of its engine's it names, granted what
    // it declares; the command's own, its engine's host's, for `host`; its loopback alone
    // for `none`; or that of one of its own containers that has none of its own.
    let network_of = |given: &str| -> Result<String, Answer> {
        match given {
            "" | "default" | "bridge" => Ok("bridge".to_string()),
            g => engine_network(networks, g)
                .map(|n| n.name.clone())
                .ok_or_else(|| Answer::error(404, format!("No such network: {g}"))),
        }
    };
    let endpoints = std::mem::take(&mut run.endpoints);
    let alone = |mode: &str| -> Result<(), Answer> {
        match endpoints.first() {
            Some(e) => Err(Answer::error(
                400,
                format!(
                    "--network {mode} with network {}: a container on {mode} is on no network of its engine's",
                    e.network
                ),
            )),
            None => Ok(()),
        }
    };
    let (joined, engine_net, nets): (String, String, Vec<Membership>) = match run.network.as_str() {
        "none" => {
            alone("none")?;
            (provider.to_string(), "none".to_string(), Vec::new())
        }
        "host" => {
            alone("host")?;
            (provider.to_string(), String::new(), Vec::new())
        }
        mode => match mode.strip_prefix("container:") {
            Some(other) => {
                alone(mode)?;
                match resolve(scoped, other) {
                    Some(c) => (c.id.clone(), String::new(), Vec::new()),
                    None => return Err(no_such(other)),
                }
            }
            None => {
                // The network its mode names, then each its endpoints name: the address
                // its client asked for there, its aliases there, and its priority for
                // the default route, the highest's first.
                let mut nets = vec![Membership::of(network_of(mode)?)];
                for e in endpoints {
                    let refused =
                        |what: &str| Err(Answer::error(400, format!("network {}: {what}", e.network)));
                    if !e.mac.is_empty() {
                        return refused(
                            "a member of its microVM's switch takes the hardware address its kernel gives its link, and none other",
                        );
                    }
                    if !e.link_local.is_empty() {
                        return refused(
                            "a member of its microVM's switch has no link-local addresses of its own",
                        );
                    }
                    if !e.driver_opts.is_empty() {
                        return refused("a network of a microVM's own engine takes no driver options");
                    }
                    if !e.ipv6.is_empty() {
                        return refused("IPv6 between a microVM's engine's containers is not built yet");
                    }
                    let name = network_of(&e.network)?;
                    if name == "bridge" && !e.aliases.is_empty() {
                        return refused(
                            "its engine's default network names no peers, so no alias of a member's is resolved there",
                        );
                    }
                    let at = match nets.iter().position(|m| m.network == name) {
                        Some(at) => at,
                        None => {
                            nets.push(Membership::of(name));
                            nets.len() - 1
                        }
                    };
                    if let Some(entry) = nets.get_mut(at) {
                        entry.addr = e.ipv4.clone();
                        for alias in e.aliases {
                            if !entry.aliases.contains(&alias) {
                                entry.aliases.push(alias);
                            }
                        }
                        entry.priority = e.gw_priority;
                    }
                }
                nets.sort_by_key(|m| std::cmp::Reverse(m.priority));
                let first = nets.first().map(|m| m.network.clone()).unwrap_or_default();
                (provider.to_string(), first, nets)
            }
        },
    };
    run.network = format!("container:{joined}");
    run.engine_net = engine_net;
    run.engine_nets = nets
        .iter()
        .map(|m| {
            let mut entry = format!("{}\0{}", m.network, m.addr);
            for alias in &m.aliases {
                entry.push('\0');
                entry.push_str(alias);
            }
            entry
        })
        .collect();
    // A member reaches nothing of the command's, its engine's host, which reaches it on the
    // ports it publishes: the host's gateway is no address of its.
    if !run.engine_nets.is_empty()
        && let Some(given) = run.add_hosts.iter().find(|h| h.ends_with(":host-gateway"))
    {
        return Err(Answer::error(
            403,
            format!(
                "--add-host {given}: a container its microVM's engine makes reaches nothing of that microVM's command, its engine's host, which reaches it on the ports it publishes"
            ),
        ));
    }
    // A bind names a path of its microVM's own command's files, as a bind names one of its
    // engine's host's: init copies it from the command's mount namespace into the
    // container's (`own-bind`), and nothing of the host's. A volume, named or not, is the
    // microVM's own (the store's, found by the name its engine gave it), and a tmpfs its
    // own; a volume takes no driver options, the local driver's binding a host directory.
    let mut binds = Vec::new();
    for b in std::mem::take(&mut run.binds) {
        let point = crate::volumes::parse_raw(&b, "").map_err(|e| Answer::error(400, e))?;
        match own_bind(&point) {
            Some(Ok(bind)) => run.own_binds.push(bind),
            Some(Err(why)) => return Err(Answer::error(400, format!("-v {b}: {why}"))),
            None if point.kind == "volume" => binds.push(b),
            None => return past(format!("-v {b}")),
        }
    }
    run.binds = binds;
    let mut kept = Vec::new();
    for m in std::mem::take(&mut run.mounts) {
        let point = crate::volumes::parse_spec_within(&m).map_err(|e| Answer::error(400, e))?;
        match (point.kind.as_str(), own_bind(&point)) {
            ("bind", Some(Ok(bind))) => run.own_binds.push(bind),
            ("bind", Some(Err(why))) => return Err(Answer::error(400, format!("--mount {m}: {why}"))),
            ("tmpfs", _) => kept.push(m),
            ("volume", _) => {
                let driver = shards_cmdline::mounts::parse_mount(&m, None)
                    .ok()
                    .and_then(|p| p.volume)
                    .and_then(|v| v.driver);
                if driver
                    .is_some_and(|(name, opts)| !opts.is_empty() || !matches!(name.as_str(), "" | "local"))
                {
                    return Err(Answer::error(
                        400,
                        format!(
                            "--mount {m}: a volume of a microVM's own engine takes no driver or driver options: the local driver's bind a directory of its engine's host"
                        ),
                    ));
                }
                kept.push(m);
            }
            _ => return past(format!("--mount {m}")),
        }
    }
    run.mounts = kept;
    if !matches!(run.volume_driver.as_str(), "" | "local") {
        return past(format!("--volume-driver {}", run.volume_driver));
    }
    for v in &mut run.volumes_from {
        let (name, mode) = v
            .split_once(':')
            .map_or((v.as_str(), None), |(n, m)| (n, Some(m)));
        let Some(c) = resolve(scoped, name) else {
            return Err(no_such(name));
        };
        *v = match mode {
            Some(m) => format!("{}:{m}", c.id),
            None => c.id.clone(),
        };
    }
    // Namespaces of its own containers, by their IDs; the microVM's own are its host's.
    for mode in [&mut run.pid, &mut run.ipc] {
        if let Some(other) = mode.strip_prefix("container:") {
            match resolve(scoped, other) {
                Some(c) => *mode = format!("container:{}", c.id),
                None => return Err(no_such(other)),
            }
        }
    }
    // The host's devices: its GPUs, its CDI devices, a VFIO group's functions.
    if !run.device_requests.is_empty() {
        return past("--gpus".into());
    }
    if let Some(d) = run.cdi_devices.first() {
        return past(format!("--device {d}"));
    }
    if let Some(d) = run.devices.iter().find(|d| d.starts_with("/dev/vfio")) {
        return past(format!("--device {d}"));
    }
    // A log is kept where it was made; the drivers that send one elsewhere send it from
    // the host, past the microVM's network grants.
    if !matches!(run.log_driver.as_str(), "" | "json-file" | "local" | "none") {
        return past(format!("--log-driver {}", run.log_driver));
    }
    if !run.runtime.is_empty() {
        return past(format!("--runtime {}", run.runtime));
    }
    // The docker CLI's own --use-api-socket, a bind of its engine's socket, is refused
    // above; and so is one asked of this engine as shards asks it.
    if run.use_api_socket {
        return Err(Answer::error(
            403,
            "--use-api-socket: a container made by a microVM's own engine has no engine of its own",
        ));
    }
    // Its processes are its microVM's, under that microVM's parent on the host (D183); a
    // parent of its own within the microVM is to come.
    if !run.cgroup_parent.is_empty() {
        return Err(Answer::error(
            501,
            format!(
                "--cgroup-parent {}: a parent of its own, within the microVM, for a container its engine makes is not built yet",
                run.cgroup_parent
            ),
        ));
    }
    Ok(())
}

impl<D: Disk> Daemon<D> {
    /// The containers microVM `scope`'s engine made, each by the name it has there.
    pub(super) fn scoped(&self, scope: &Scope) -> Vec<Container> {
        of_scope(lock(&self.containers).all(), &scope.provider)
    }

    /// `call` answered as microVM `scope`'s own engine answers it, made on `conn` itself
    /// where it streams.
    pub(super) fn route_scoped<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &std::os::unix::net::UnixStream,
        call: &Call<'_>,
        scope: &Scope,
    ) -> Done {
        let path: Vec<&str> = call.path.iter().map(String::as_str).collect();
        let method = call.head.method.as_str();
        let answer = match (method, path.as_slice()) {
            ("GET" | "HEAD", ["_ping"]) => super::ping(method),
            ("GET", ["version"]) => super::version(),
            ("GET", ["info"]) => self.scoped_info(scope),
            ("POST", ["containers", "create"]) => self.scoped_create(threads, call, scope),
            ("GET", ["containers", "json"]) => self.scoped_list(call, scope),
            ("POST", ["images", "create"]) => return self.scoped_pull(threads, conn, call, scope),
            ("GET", ["networks"]) => self.scoped_networks(call, scope),
            ("POST", ["networks", "create"]) => self.scoped_network_create(call, scope),
            ("POST", ["networks", "prune"]) => self.scoped_network_prune(call, scope),
            ("GET", ["networks", term]) => self.scoped_network(term, scope),
            ("DELETE", ["networks", term]) => self.scoped_network_remove(term, scope),
            ("POST", ["networks", term, "connect"]) => self.scoped_network_join(term, call, scope, true),
            ("POST", ["networks", term, "disconnect"]) => self.scoped_network_join(term, call, scope, false),
            ("GET", ["volumes"]) => self.scoped_volumes(call, scope),
            ("POST", ["volumes", "create"]) => self.scoped_volume_create(call, scope),
            ("POST", ["volumes", "prune"]) => self.scoped_volume_prune(call, scope),
            ("GET", ["volumes", name]) => self.scoped_volume(name, scope),
            ("DELETE", ["volumes", name]) => self.scoped_volume_remove(name, call, scope),
            ("GET", ["images", "json"]) => self.scoped_images(call, scope),
            ("GET", ["images", name @ .., "json"]) if !name.is_empty() => {
                self.scoped_image(threads, &name.join("/"), scope)
            }
            (_, ["containers", given, rest @ ..]) => {
                self.settle();
                let scoped = self.scoped(scope);
                let Some(c) = resolve(&scoped, given) else {
                    return Done::Answer(no_such(given));
                };
                let id = c.id.clone();
                match (method, rest) {
                    ("GET", ["json"]) => Answer::document(
                        200,
                        self.scoped_mounts(crate::daemon::inspect_doc::json(
                            &self.container_value(c, call.flag("size")),
                        )),
                    ),
                    ("POST", ["start"]) => self.engine_start(threads, &id, call),
                    ("POST", ["stop"]) => self.engine_stop(threads, &id, call, false),
                    ("POST", ["kill"]) => self.engine_stop(threads, &id, call, true),
                    ("DELETE", []) => self.engine_remove(threads, &id, call),
                    ("POST", ["wait"]) => return self.engine_wait(conn, &id, call),
                    ("GET", ["logs"]) => return self.engine_logs(threads, conn, &id, call),
                    ("POST", ["attach"]) => return self.engine_attach(threads, conn, &id, call),
                    ("POST", ["resize"]) => self.engine_resize(&id, call),
                    ("POST", ["exec"]) => self.engine_exec_create(&id, call),
                    _ => unserved(method, &call.head.target),
                }
            }
            // An exec of its own containers' alone, said as one that is not there.
            (_, ["exec", exec_id, rest @ ..]) => {
                self.settle();
                let scoped = self.scoped(scope);
                let ours = self
                    .exec_container(exec_id)
                    .is_some_and(|c| scoped.iter().any(|s| s.id == c));
                if !ours {
                    return Done::Answer(Answer::error(404, format!("No such exec instance: {exec_id}")));
                }
                match (method, rest) {
                    ("POST", ["start"]) => return self.engine_exec_start(threads, conn, exec_id, call),
                    ("POST", ["resize"]) => self.engine_resize(exec_id, call),
                    ("GET", ["json"]) => self.engine_exec_inspect(exec_id),
                    _ => unserved(method, &call.head.target),
                }
            }
            _ => unserved(method, &call.head.target),
        };
        Done::Answer(answer)
    }

    /// `GET /info` of a microVM's own engine: its own containers and image, and the
    /// microVM's CPUs and memory, where its run limits them; nothing of its host's that
    /// the microVM has no part of (its name, its directories, its proxies).
    fn scoped_info(&self, scope: &Scope) -> Answer {
        let scoped = self.scoped(scope);
        let paused = lock(&self.paused).clone();
        let running = scoped.iter().filter(|c| c.state == Life::Running).count();
        let paused = scoped.iter().filter(|c| paused.contains(&c.id)).count();
        let provider = lock(&self.containers).get(&scope.provider).cloned();
        let made_by = self.made_by(&scope.provider);
        let limits = made_by.as_ref().map(|r| &r.resources);
        let facts = self.facts().scoped(crate::daemon::info::Scoped {
            containers: scoped.len(),
            running: running.saturating_sub(paused),
            paused,
            // Its own images: its microVM's, and those its engine pulled.
            images: provider.as_ref().map_or(0, |p| images_of(p).len()),
            cpus: limits
                .and_then(|l| u64::try_from(l.nano_cpus).ok())
                .filter(|&n| n > 0)
                .map(|n| usize::try_from(n.div_ceil(1_000_000_000)).unwrap_or(usize::MAX)),
            memory: limits
                .map(|l| l.memory)
                .filter(|&m| m > 0)
                .and_then(|m| u64::try_from(m).ok()),
            name: made_by
                .as_ref()
                .and_then(|r| r.hostname.clone())
                .or_else(|| provider.as_ref().map(|c| c.id.chars().take(12).collect())),
        });
        let info = crate::daemon::info::server(&facts, "system.Info").value();
        Answer::document(200, crate::daemon::inspect_doc::json(&info))
    }

    /// `POST /containers/create` of a microVM's own engine: a joiner of its microVM, of the
    /// microVM's own image, named in the scope as its client names it, and on the host
    /// after the microVM's own name.
    fn scoped_create<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        call: &Call<'_>,
        scope: &Scope,
    ) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a create's body: {e}")),
        };
        let image = body.get("Image").and_then(Value::as_str).unwrap_or_default();
        let made = create::run_of(
            &body,
            None,
            call.query("platform").filter(|p| !p.is_empty()),
            || self.image_cmd(threads, image),
        );
        let mut run = match made {
            Ok(run) => run,
            Err(e) => return Answer::error(status_of(&e, 400), e),
        };
        self.settle();
        let scoped = self.scoped(scope);
        let provider = lock(&self.containers).get(&scope.provider).cloned();
        let Some(provider) = provider else {
            return Answer::error(409, "its microVM is gone");
        };
        if let Err(answer) = confine(&mut run, &scope.provider, &scoped, &provider.engine_networks) {
            return answer;
        }
        // Its own images alone: its microVM's, and those its engine pulled.
        match self.image_id_of(threads, image) {
            Some(id) if images_of(&provider).contains(&id) => {}
            _ => return Answer::error(404, format!("No such image: {image}")),
        }
        // Its name in the scope, and on the host after its microVM's.
        let asked = call
            .query("name")
            .map(|n| n.strip_prefix('/').unwrap_or(n))
            .filter(|n| !n.is_empty());
        let there = match asked {
            Some(name) if !crate::containers::valid_name(name) => {
                return Answer::error(
                    400,
                    format!("Invalid container name ({name}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"),
                );
            }
            Some(name) if scoped.iter().any(|c| c.name == name) => {
                return Answer::error(
                    409,
                    format!("Conflict. The container name \"/{name}\" is already in use"),
                );
            }
            Some(name) => {
                run.name = Some(format!("{}.{name}", provider.name));
                name.to_string()
            }
            None => String::new(),
        };
        run.engine_made = Some((scope.provider.clone(), there));
        if let Err(e) = self.as_client(&mut run) {
            return Answer::error(500, e);
        }
        // Its registries and clouds are its run's client's to reach, not the daemon's.
        run.registry_env = Vec::new();
        run.cloud_env = Vec::new();
        match self.run_detached(threads, &run) {
            Ok(super::RunAnswer {
                status: 0,
                id: Some(id),
                said,
            }) => Answer::json(
                201,
                &json!({ "Id": id, "Warnings": super::containers::warnings(&said) }),
            ),
            Ok(answer) => refused(&answer.said, 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /images/create` of a microVM's own engine: a pull, its registries reached as
    /// its run's client reaches them (its credentials kept by the daemon, never in the
    /// microVM), and those a client in it gives over them; what it pulled its own after.
    fn scoped_pull<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &std::os::unix::net::UnixStream,
        call: &Call<'_>,
        scope: &Scope,
    ) -> Done {
        if call.query("fromSrc").is_some_and(|s| !s.is_empty()) {
            return Done::Answer(unserved(&call.head.method, &call.head.target));
        }
        let env = self
            .inbox_of(&scope.provider)
            .and_then(|inbox| lock(&inbox).engine.as_ref().map(|e| e.registry_env.clone()));
        let Some(env) = env else {
            return Done::Answer(Answer::error(409, "its microVM is not running"));
        };
        let (done, pulled) = self.pull_with(threads, conn, call, env);
        if let Some((reference, every_tag)) = pulled {
            let ids: Vec<String> = if every_tag {
                self.ask_command(
                    threads,
                    &["images", "--no-trunc", "--format", "{{.ID}}", &reference],
                )
                .ok()
                .filter(|(status, _, _)| *status == 0)
                .map(|(_, out, _)| {
                    String::from_utf8_lossy(&out)
                        .lines()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
            } else {
                self.image_id_of(threads, &reference).into_iter().collect()
            };
            let kept = lock(&self.containers).change(&scope.provider, |c| {
                for id in ids {
                    if !c.engine_images.contains(&id) {
                        c.engine_images.push(id);
                    }
                }
            });
            if kept.is_ok() {
                self.record_soon(&scope.provider, Vec::new());
            }
        }
        done
    }

    /// `GET /images/json` of a microVM's own engine: its own images, as the store sums
    /// them up.
    fn scoped_images(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let ours = lock(&self.containers)
            .get(&scope.provider)
            .map(images_of)
            .unwrap_or_default();
        let listed = self.engine_images(call);
        if listed.status != 200 {
            return listed;
        }
        let all: Value = serde_json::from_slice(&listed.body).unwrap_or_default();
        let kept: Vec<Value> = all
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["Id"].as_str().is_some_and(|id| ours.iter().any(|o| o == id)))
            .cloned()
            .collect();
        Answer::json(200, &Value::Array(kept))
    }

    /// `GET /images/{name}/json` of a microVM's own engine: one of its own images.
    fn scoped_image<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, name: &str, scope: &Scope) -> Answer {
        let ours = lock(&self.containers)
            .get(&scope.provider)
            .map(images_of)
            .unwrap_or_default();
        match self.image_id_of(threads, name) {
            Some(id) if ours.contains(&id) => self.engine_image_inspect(name),
            _ => Answer::error(404, format!("No such image: {name}")),
        }
    }

    /// The ID image reference `image` names in the store, where it names one.
    fn image_id_of<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, image: &str) -> Option<String> {
        let (status, out, _) = self
            .ask_command(threads, &["image", "inspect", "--format", "{{.Id}}", image])
            .ok()?;
        (status == 0).then(|| String::from_utf8_lossy(&out).trim().to_string())
    }

    /// `GET /containers/json` of a microVM's own engine: its own containers, as `ps` lists
    /// them, by their names there.
    fn scoped_list(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let limit = call
            .query("limit")
            .and_then(|l| l.parse::<i64>().ok())
            .filter(|l| *l > 0)
            .and_then(|l| usize::try_from(l).ok());
        let mut argv: Vec<String> = Vec::new();
        // `-n` lists stopped ones too, as dockerd's limit does.
        if call.flag("all") || limit.is_some() {
            argv.push("--all".into());
        }
        match filters(call.query("filters")) {
            Ok(f) => {
                for (k, v) in f {
                    argv.extend(["--filter".into(), format!("{k}={v}")]);
                }
            }
            Err(e) => return Answer::error(400, e),
        }
        let parsed = match flags::parse(&shards_cmdline::commands::PS, "shards ps", &argv, &flags::value) {
            Outcome::Run(parsed) => parsed,
            Outcome::Fail { text, .. } => return Answer::error(400, plain(&text)),
            Outcome::Help { .. } => return Answer::error(400, "a list's query asks for help"),
        };
        self.settle();
        let listing = match self.ps_listing(&parsed) {
            Ok(listing) => listing,
            Err(e) => return refused(&e, 400),
        };
        let sized = call.flag("size");
        let summaries: Vec<Value> = of_scope(listing.containers.iter(), &scope.provider)
            .iter()
            .take(limit.unwrap_or(usize::MAX))
            .map(|c| self.summary(c, sized))
            .collect();
        Answer::json(200, &Value::Array(summaries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, name: &str, of: Option<(&str, &str)>) -> Container {
        Container {
            id: id.into(),
            name: name.into(),
            image: String::new(),
            command: Vec::new(),
            created: 0,
            state: Life::Created,
            started: None,
            finished: None,
            exit_code: None,
            error: String::new(),
            auto_remove: false,
            log_lost: 0,
            log_dropped: 0,
            log_compress: false,
            log_ships: false,
            stop_signal: None,
            stop_timeout: None,
            ports: Vec::new(),
            image_id: None,
            labels: Default::default(),
            annotations: Default::default(),
            oom_killed: false,
            mounts: Vec::new(),
            restart: Default::default(),
            size_rw: None,
            saved: None,
            engine_made: of.map(|(of, name)| crate::containers::EngineMade {
                of: of.into(),
                name: name.into(),
                addresses: Vec::new(),
            }),
            engine_images: Vec::new(),
            engine_networks: Vec::new(),
        }
    }

    /// A scope sees what its engine made alone, by the names it gave them, and resolves
    /// them as dockerd resolves a reference; another microVM's, and the host's own, are
    /// not there for it.
    #[test]
    fn a_scope_sees_its_own_containers_by_their_names_there() {
        let all = [
            container("aaa111", "prov.db", Some(("prov-id", "db"))),
            container("aab222", "prov.web", Some(("prov-id", "web"))),
            container("ccc333", "other.db", Some(("other-id", "db"))),
            container("ddd444", "db", None),
        ];
        let scoped = of_scope(all.iter(), "prov-id");
        let names: Vec<&str> = scoped.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["db", "web"]);
        let found = |given: &str| resolve(&scoped, given).map(|c| c.id.as_str());
        assert_eq!(found("db"), Some("aaa111"));
        assert_eq!(found("/web"), Some("aab222"));
        assert_eq!(found("aaa111"), Some("aaa111"));
        assert_eq!(found("aab"), Some("aab222"));
        // A prefix two share, another microVM's, and the host's own: none.
        assert_eq!(found("aa"), None);
        assert_eq!(found("ccc333"), None);
        assert_eq!(found("ddd444"), None);
        assert_eq!(found(""), None);
    }

    fn refused_why(run: &mut Run, scoped: &[Container]) -> Option<(u16, String)> {
        confine(run, "prov-id", scoped, &[]).err().map(|a| {
            let said: Value = serde_json::from_slice(&a.body).unwrap_or_default();
            (a.status, said["message"].as_str().unwrap_or_default().to_string())
        })
    }

    /// A create joins its microVM, whatever network it asks for there, and what would
    /// reach past it is refused, naming what asked for it.
    #[test]
    fn a_create_stays_in_its_microvm() {
        let scoped = of_scope(
            [container("aaa111", "prov.db", Some(("prov-id", "db")))].iter(),
            "prov-id",
        );
        for (given, own) in [
            ("", "bridge"),
            ("default", "bridge"),
            ("bridge", "bridge"),
            ("none", "none"),
            ("host", ""),
        ] {
            let mut run = Run {
                network: given.into(),
                ..Run::default()
            };
            assert_eq!(refused_why(&mut run, &scoped), None, "{given:?}");
            assert_eq!(
                (run.network.as_str(), run.engine_net.as_str()),
                ("container:prov-id", own)
            );
        }
        let mut run = Run {
            network: "container:db".into(),
            pid: "container:db".into(),
            volumes_from: vec!["db:ro".into()],
            volumes: vec!["/data".into()],
            mounts: vec!["type=tmpfs,target=/t".into(), "type=volume,target=/v".into()],
            ..Run::default()
        };
        assert_eq!(refused_why(&mut run, &scoped), None);
        // Binds name its microVM's own files: `-v`'s made where missing, as dockerd makes a
        // host's, `--mount`'s not.
        let mut bound = Run {
            binds: vec!["/work:/src:ro".into()],
            mounts: vec!["type=bind,source=/w,target=/x".into()],
            ..Run::default()
        };
        assert_eq!(refused_why(&mut bound, &scoped), None);
        assert_eq!(
            (bound.binds.len(), bound.mounts.len(), bound.own_binds.as_slice()),
            (
                0,
                0,
                ["/work\0/src\0ro,create".to_string(), "/w\0/x\0rw".to_string()].as_slice()
            )
        );
        // A named volume is the microVM's own, kept by the name its client gave it.
        let mut named = Run {
            binds: vec!["data:/var/lib/data:ro".into()],
            mounts: vec!["type=volume,source=cache,target=/cache".into()],
            ..Run::default()
        };
        assert_eq!(refused_why(&mut named, &scoped), None);
        assert_eq!(
            (named.binds.as_slice(), named.mounts.as_slice()),
            (
                ["data:/var/lib/data:ro".to_string()].as_slice(),
                ["type=volume,source=cache,target=/cache".to_string()].as_slice()
            )
        );
        assert_eq!(
            (
                run.network.as_str(),
                run.pid.as_str(),
                run.volumes_from.as_slice()
            ),
            (
                "container:aaa111",
                "container:aaa111",
                ["aaa111:ro".to_string()].as_slice()
            )
        );
        let past = |what: &str| {
            Some((
                403,
                format!(
                    "{what}: a container a microVM's own engine makes is that microVM's, and takes nothing of its host's"
                ),
            ))
        };
        let cases: Vec<(Run, Option<(u16, String)>)> = vec![
            (
                // A network its engine has none of, by that name.
                Run {
                    network: "elsewhere".into(),
                    ..Run::default()
                },
                Some((404, "No such network: elsewhere".into())),
            ),
            (
                Run {
                    binds: vec!["/w:/x:rshared".into()],
                    ..Run::default()
                },
                Some((
                    400,
                    "-v /w:/x:rshared: propagation rshared: a container's copy of its microVM's own files shares no mount with the command's".into(),
                )),
            ),
            (
                Run {
                    mounts: vec!["source=vol,target=/x,volume-opt=type=none,volume-opt=o=bind,volume-opt=device=/etc".into()],
                    ..Run::default()
                },
                Some((
                    400,
                    "--mount source=vol,target=/x,volume-opt=type=none,volume-opt=o=bind,volume-opt=device=/etc: a volume of a microVM's own engine takes no driver or driver options: the local driver's bind a directory of its engine's host".into(),
                )),
            ),
            (
                Run {
                    log_driver: "syslog".into(),
                    ..Run::default()
                },
                past("--log-driver syslog"),
            ),
            (
                Run {
                    runtime: "runc".into(),
                    ..Run::default()
                },
                past("--runtime runc"),
            ),
            (
                Run {
                    devices: vec!["/dev/vfio/12:/dev/vfio/12:rwm".into()],
                    ..Run::default()
                },
                past("--device /dev/vfio/12:/dev/vfio/12:rwm"),
            ),
            (
                Run {
                    network: "container:elsewhere".into(),
                    ..Run::default()
                },
                Some((404, "No such container: elsewhere".into())),
            ),
            (
                Run {
                    volumes_from: vec!["ddd444".into()],
                    ..Run::default()
                },
                Some((404, "No such container: ddd444".into())),
            ),
            (
                Run {
                    use_api_socket: true,
                    ..Run::default()
                },
                Some((
                    403,
                    "--use-api-socket: a container made by a microVM's own engine has no engine of its own"
                        .into(),
                )),
            ),
            (
                Run {
                    cgroup_parent: "agents.slice".into(),
                    ..Run::default()
                },
                Some((
                    501,
                    "--cgroup-parent agents.slice: a parent of its own, within the microVM, for a container its engine makes is not built yet"
                        .into(),
                )),
            ),
        ];
        for (mut run, said) in cases {
            assert_eq!(refused_why(&mut run, &scoped), said);
        }
    }
}
