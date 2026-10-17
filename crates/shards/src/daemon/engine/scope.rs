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
            | ("POST", ["containers", _, "start" | "stop" | "kill" | "wait"])
            | ("DELETE", ["containers", _])
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
             and its own containers' create, list, inspect, start, stop, kill, wait, logs and remove"
        ),
    )
}

/// A container a scope cannot see, said as dockerd says one that is not there.
fn no_such(given: &str) -> Answer {
    Answer::error(404, format!("No such container: {given}"))
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
fn resolve<'a>(scoped: &'a [Container], given: &str) -> Option<&'a Container> {
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

/// What of a create's run would reach past its microVM, refused with why; what names a
/// container is taken in the scope (`scoped`), as its engine's clients name them.
fn confine(run: &mut Run, provider: &str, scoped: &[Container]) -> Result<(), Answer> {
    let past = |what: String| {
        Err(Answer::error(
            403,
            format!(
                "{what}: a container made by a microVM's own engine is in that microVM, and reaches nothing past it"
            ),
        ))
    };
    // Its network is its microVM's: a joiner of it, as the microVM's own network, its
    // host's, is.
    let joined = match run.network.as_str() {
        "" | "default" | "host" => provider.to_string(),
        mode => match mode.strip_prefix("container:") {
            Some(other) => match resolve(scoped, other) {
                Some(c) => c.id.clone(),
                None => return Err(no_such(other)),
            },
            None => return past(format!("--network {mode}")),
        },
    };
    run.network = format!("container:{joined}");
    if let Some(e) = run.endpoints.first() {
        return past(format!("--network {}", e.network));
    }
    // Binds name the host's files, and a named volume is another microVM's to share.
    if let Some(b) = run.binds.first() {
        return past(format!("-v {b}"));
    }
    for m in &run.mounts {
        let fields: Vec<(&str, &str)> = m
            .split(',')
            .map(|f| f.split_once('=').unwrap_or((f, "")))
            .collect();
        let field = |k: &[&str]| fields.iter().find(|(f, _)| k.contains(f)).map(|(_, v)| *v);
        let kind = field(&["type"]).unwrap_or("volume");
        let named = field(&["source", "src"]).is_some_and(|s| !s.is_empty());
        if !(kind == "tmpfs" || (kind == "volume" && !named)) {
            return past(format!("--mount {m}"));
        }
    }
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
    Ok(())
}

impl<D: Disk> Daemon<D> {
    /// The containers microVM `scope`'s engine made, each by the name it has there.
    fn scoped(&self, scope: &Scope) -> Vec<Container> {
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
                        crate::daemon::inspect_doc::json(&self.container_value(c, call.flag("size"))),
                    ),
                    ("POST", ["start"]) => self.engine_start(threads, &id, call),
                    ("POST", ["stop"]) => self.engine_stop(threads, &id, call, false),
                    ("POST", ["kill"]) => self.engine_stop(threads, &id, call, true),
                    ("DELETE", []) => self.engine_remove(threads, &id, call),
                    ("POST", ["wait"]) => return self.engine_wait(conn, &id, call),
                    ("GET", ["logs"]) => return self.engine_logs(threads, conn, &id, call),
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
            // Its own image: a create there runs it.
            images: usize::from(provider.is_some()),
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
        if let Err(answer) = confine(&mut run, &scope.provider, &scoped) {
            return answer;
        }
        // Its microVM's own image alone, until it pulls with its run's credentials.
        let provider = lock(&self.containers).get(&scope.provider).cloned();
        let Some(provider) = provider else {
            return Answer::error(409, "its microVM is gone");
        };
        match self.image_id_of(threads, image) {
            Some(id) if Some(&id) == provider.image_id.as_ref() => {}
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
            }),
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
        confine(run, "prov-id", scoped).err().map(|a| {
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
        for given in ["", "default", "host"] {
            let mut run = Run {
                network: given.into(),
                ..Run::default()
            };
            assert_eq!(refused_why(&mut run, &scoped), None, "{given:?}");
            assert_eq!(run.network, "container:prov-id");
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
                    "{what}: a container made by a microVM's own engine is in that microVM, and reaches nothing past it"
                ),
            ))
        };
        let cases: Vec<(Run, Option<(u16, String)>)> = vec![
            (
                Run {
                    network: "bridge".into(),
                    ..Run::default()
                },
                past("--network bridge"),
            ),
            (
                Run {
                    network: "none".into(),
                    ..Run::default()
                },
                past("--network none"),
            ),
            (
                Run {
                    binds: vec!["/etc:/x".into()],
                    ..Run::default()
                },
                past("-v /etc:/x"),
            ),
            (
                Run {
                    binds: vec!["vol:/x".into()],
                    ..Run::default()
                },
                past("-v vol:/x"),
            ),
            (
                Run {
                    mounts: vec!["type=bind,source=/etc,target=/x".into()],
                    ..Run::default()
                },
                past("--mount type=bind,source=/etc,target=/x"),
            ),
            (
                Run {
                    mounts: vec!["source=vol,target=/x".into()],
                    ..Run::default()
                },
                past("--mount source=vol,target=/x"),
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
        ];
        for (mut run, said) in cases {
            assert_eq!(refused_why(&mut run, &scoped), said);
        }
    }
}
