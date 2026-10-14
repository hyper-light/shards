//! `docker commit --change` and `docker import --change`: Dockerfile instructions applied
//! to a container's config as moby's dockerd applies them (moby 464cd50c,
//! daemon/builder/dockerfile builder.go `BuildFromConfig`). The changes are parsed as one
//! Dockerfile by BuildKit's parser and typed as its `ParseCommand` types them (`parser`,
//! `instructions`), every one checked before any is applied, and each is then applied as
//! the classic builder's dispatchers apply it (dispatchers.go, evaluator.go `dispatch`):
//! not as BuildKit's plan does, so `WORKDIR` makes no directory and the shell form of
//! `CMD` and `ENTRYPOINT` takes the config's `Shell`.
//!
//! The config is a `container.Config` as the API writes it, and comes back so. The
//! dispatchers are those of dockerd built for Linux, the daemon whose containers shards
//! runs, on every host: paths are slash-separated, the default shell is `/bin/sh -c`,
//! environment names are case-sensitive and `STOPSIGNAL` takes Linux's signals
//! (moby/sys/signal signal_linux.go). Held to moby by tests/commit.rs, against what
//! scripts/commit-changes/generate records dockerd making of its cases
//! (testdata/commit-changes.json); it inherits `lex`'s one deliberate difference.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::go;
use crate::instructions::{self, CmdLine, Health, KeyValue, Kind};
use crate::lex::{EnvList, Lex};
use crate::parser;

/// The instructions a change may be: `validCommitCommands`.
const VALID: [&[u8]; 11] = [
    b"cmd",
    b"entrypoint",
    b"healthcheck",
    b"env",
    b"expose",
    b"label",
    b"onbuild",
    b"stopsignal",
    b"user",
    b"volume",
    b"workdir",
];

/// `config` with `changes` applied for a container of `os`, or moby's error. With no
/// changes the config comes back as it is.
pub fn build_from_config(config: &Value, changes: &[String], os: &str) -> Result<Value, String> {
    if changes.is_empty() {
        return Ok(config.clone());
    }
    let parsed = parser::parse(changes.join("\n").as_bytes()).map_err(|e| text(&e.message))?;
    let mut commands = Vec::with_capacity(parsed.instructions.len());
    for node in &parsed.instructions {
        if !VALID.contains(&go::to_lower(&node.value).as_slice()) {
            return Err(format!("{} is not a valid change command", text(&node.value)));
        }
        commands.push(instructions::parse_command(node).map_err(|e| text(&e.message))?);
    }
    let Value::Object(fields) = config else {
        return Err("the config is not a JSON object".into());
    };
    let mut state = State::read(fields)?;
    let lex = Lex::new(u32::from(parsed.escape));
    for command in commands {
        state.dispatch(&lex, command.kind, os)?;
    }
    let mut out = fields.clone();
    state.write(&mut out);
    Ok(Value::Object(out))
}

/// A key and value, expanded.
type Pair = (Vec<u8>, Vec<u8>);

/// The fields of the config the changes can read or set: `dispatchState.runConfig`.
/// `None` is Go's nil.
struct State {
    user: Vec<u8>,
    env: Option<Vec<Vec<u8>>>,
    cmd: Option<Vec<Vec<u8>>>,
    entrypoint: Option<Vec<Vec<u8>>>,
    shell: Vec<Vec<u8>>,
    args_escaped: bool,
    working_dir: Vec<u8>,
    exposed_ports: Option<BTreeSet<String>>,
    volumes: Option<BTreeSet<String>>,
    labels: Option<Map<String, Value>>,
    on_build: Option<Vec<Vec<u8>>>,
    stop_signal: Vec<u8>,
    healthcheck: Option<Value>,
    /// Whether a `CMD` with arguments came before, so `ENTRYPOINT` keeps it.
    cmd_set: bool,
}

impl State {
    fn read(c: &Map<String, Value>) -> Result<State, String> {
        let set = |key: &str| -> Result<Option<BTreeSet<String>>, String> {
            match c.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Object(m)) => Ok(Some(m.keys().cloned().collect())),
                Some(_) => Err(format!("the config's {key} is not an object")),
            }
        };
        Ok(State {
            user: string(c, "User")?,
            env: strings(c, "Env")?,
            cmd: strings(c, "Cmd")?,
            entrypoint: strings(c, "Entrypoint")?,
            shell: strings(c, "Shell")?.unwrap_or_default(),
            args_escaped: matches!(c.get("ArgsEscaped"), Some(Value::Bool(true))),
            working_dir: string(c, "WorkingDir")?,
            exposed_ports: set("ExposedPorts")?,
            volumes: set("Volumes")?,
            labels: match c.get("Labels") {
                None | Some(Value::Null) => None,
                Some(Value::Object(m)) => Some(m.clone()),
                Some(_) => return Err("the config's Labels is not an object".into()),
            },
            on_build: strings(c, "OnBuild")?,
            stop_signal: string(c, "StopSignal")?,
            healthcheck: c.get("Healthcheck").filter(|v| !v.is_null()).cloned(),
            cmd_set: false,
        })
    }

    /// The fields into `c` as `json.Marshal` writes them, `omitempty` ones left out when
    /// empty.
    fn write(self, c: &mut Map<String, Value>) {
        let set = |s: BTreeSet<String>| {
            Value::Object(s.into_iter().map(|k| (k, Value::Object(Map::new()))).collect())
        };
        let mut omit = |key: &str, v: Option<Value>| match v {
            Some(v) => c.insert(key.into(), v),
            None => c.remove(key),
        };
        omit(
            "ExposedPorts",
            self.exposed_ports.filter(|p| !p.is_empty()).map(set),
        );
        omit("Healthcheck", self.healthcheck);
        omit("ArgsEscaped", self.args_escaped.then_some(Value::Bool(true)));
        omit(
            "OnBuild",
            self.on_build.filter(|o| !o.is_empty()).map(|o| list(&o)),
        );
        omit(
            "StopSignal",
            (!self.stop_signal.is_empty()).then(|| text(&self.stop_signal).into()),
        );
        let nullable = |v: Option<&Vec<Vec<u8>>>| v.map_or(Value::Null, |v| list(v));
        c.insert("User".into(), text(&self.user).into());
        c.insert("Env".into(), nullable(self.env.as_ref()));
        c.insert("Cmd".into(), nullable(self.cmd.as_ref()));
        c.insert("Entrypoint".into(), nullable(self.entrypoint.as_ref()));
        c.insert("WorkingDir".into(), text(&self.working_dir).into());
        c.insert("Volumes".into(), self.volumes.map_or(Value::Null, set));
        c.insert("Labels".into(), self.labels.map_or(Value::Null, Value::Object));
    }

    /// One change applied: evaluator.go `dispatch`, then the instruction's dispatcher.
    fn dispatch(&mut self, lex: &Lex, kind: Kind, os: &str) -> Result<(), String> {
        // `PlatformSpecific`: StopSignalCommand.CheckPlatform.
        if matches!(kind, Kind::StopSignal(_)) && os == "windows" {
            return Err("The daemon on this platform does not support the command stopsignal".into());
        }
        let env = EnvList::from_entries(self.env.iter().flatten().map(Vec::as_slice));
        let expand = |word: &[u8]| -> Result<Vec<u8>, String> {
            lex.process(word, &env).map(|p| p.word).map_err(|e| text(&e.0))
        };
        let expand_kvs = |kvs: Vec<KeyValue>| -> Result<Vec<Pair>, String> {
            kvs.into_iter()
                .map(|kv| Ok((expand(&kv.key)?, expand(&kv.value)?)))
                .collect()
        };
        match kind {
            // dispatchEnv: each replaces the first entry of its name, or is added.
            Kind::Env(kvs) => {
                let env = self.env.get_or_insert_default();
                for (key, value) in expand_kvs(kvs)? {
                    let entry = [key.as_slice(), b"=", &value].concat();
                    let name = |e: &Vec<u8>| e.split(|&b| b == b'=').next() == Some(key.as_slice());
                    match env.iter_mut().find(|e| name(e)) {
                        Some(e) => *e = entry,
                        None => env.push(entry),
                    }
                }
            }
            Kind::Label(kvs) => {
                let labels = self.labels.get_or_insert_default();
                for (key, value) in expand_kvs(kvs)? {
                    labels.insert(text(&key), text(&value).into());
                }
            }
            Kind::Onbuild(expression) => self.on_build.get_or_insert_default().push(expression),
            Kind::Workdir(path) => {
                self.working_dir = normalize_workdir(&self.working_dir, &expand(&path)?)?;
            }
            Kind::Cmd(c) => {
                self.cmd = self.resolve(&c);
                self.args_escaped = false;
                if !c.cmd_line.is_empty() {
                    self.cmd_set = true;
                }
            }
            Kind::Entrypoint(c) => {
                self.entrypoint = self.resolve(&c);
                self.args_escaped = false;
                if !self.cmd_set {
                    self.cmd = None;
                }
            }
            Kind::Healthcheck(h) => self.healthcheck = Some(health(&h)),
            // dispatchExpose: each word expanded into words, the only instruction so.
            Kind::Expose(ports) => {
                let mut words = Vec::new();
                for p in &ports {
                    words.extend(lex.process(p, &env).map_err(|e| text(&e.0))?.words);
                }
                let mut exposed = BTreeSet::new();
                for w in &words {
                    exposed.extend(port_spec(&text(w))?);
                }
                self.exposed_ports.get_or_insert_default().extend(exposed);
            }
            Kind::User(user) => self.user = expand(&user)?,
            Kind::Volume(volumes) => {
                let volumes = volumes.iter().map(|v| expand(v)).collect::<Result<Vec<_>, _>>()?;
                let set = self.volumes.get_or_insert_default();
                for v in volumes {
                    if v.is_empty() {
                        return Err("VOLUME specified can not be an empty string".into());
                    }
                    set.insert(text(&v));
                }
            }
            Kind::StopSignal(signal) => {
                let signal = expand(&signal)?;
                parse_signal(&signal)?;
                self.stop_signal = signal;
            }
            other => return Err(format!("unsupported command type: {other:?}")),
        }
        Ok(())
    }

    /// dispatchers_unix.go `resolveCmdLine`: the shell form after the config's shell, or
    /// `/bin/sh -c`.
    fn resolve(&self, c: &CmdLine) -> Option<Vec<Vec<u8>>> {
        if c.cmd_line_nil {
            return None;
        }
        if !c.prepend_shell {
            return Some(c.cmd_line.clone());
        }
        let shell = if self.shell.is_empty() {
            vec![b"/bin/sh".to_vec(), b"-c".to_vec()]
        } else {
            self.shell.clone()
        };
        Some(shell.into_iter().chain(c.cmd_line.iter().cloned()).collect())
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn list(items: &[Vec<u8>]) -> Value {
    Value::Array(items.iter().map(|i| text(i).into()).collect())
}

fn string(c: &Map<String, Value>, key: &str) -> Result<Vec<u8>, String> {
    match c.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(s.as_bytes().to_vec()),
        Some(_) => Err(format!("the config's {key} is not a string")),
    }
}

fn strings(c: &Map<String, Value>, key: &str) -> Result<Option<Vec<Vec<u8>>>, String> {
    let wrong = || format!("the config's {key} is not a list of strings");
    match c.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| i.as_str().map(|s| s.as_bytes().to_vec()).ok_or_else(wrong))
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err(wrong()),
    }
}

/// `HealthcheckConfig` as the API writes it: durations in nanoseconds, every field
/// `omitempty`.
fn health(h: &Health) -> Value {
    let mut m = Map::new();
    if !h.test.is_empty() {
        m.insert("Test".into(), list(&h.test));
    }
    for (key, v) in [
        ("Interval", h.interval),
        ("Timeout", h.timeout),
        ("StartPeriod", h.start_period),
        ("StartInterval", h.start_interval),
        ("Retries", h.retries),
    ] {
        if v != 0 {
            m.insert(key.into(), v.into());
        }
    }
    Value::Object(m)
}

/// dispatchers_unix.go `normalizeWorkdir`: a relative path is joined to the current one.
fn normalize_workdir(current: &[u8], requested: &[u8]) -> Result<Vec<u8>, String> {
    if requested.is_empty() {
        return Err("cannot normalize nothing".into());
    }
    if !go::is_abs(requested) {
        return Ok(go::join(&[b"/", current, requested]));
    }
    Ok(go::clean(requested))
}

/// `parsePortSpec`, moby's copy of go-connections' `nat.ParsePortSpec`: the container
/// ports `[ip:][hostPort:]containerPort[/proto]` exposes, as `port/proto`. The host side
/// is checked and dropped.
fn port_spec(raw: &str) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (mut ip, host_port, container) = match parts.as_slice() {
        [c] => (String::new(), "", *c),
        [h, c] => (String::new(), *h, *c),
        [ip, h, c] => ((*ip).to_string(), *h, *c),
        [ips @ .., h, c] => (ips.join(":"), *h, *c),
        [] => (String::new(), "", ""),
    };
    // splitProtoPort.
    let (container, proto) = container.split_once('/').unwrap_or((container, ""));
    if container.is_empty() {
        return Err(format!("no port specified: {raw}<empty>"));
    }
    let proto = if proto.is_empty() {
        "tcp".to_string()
    } else {
        proto.to_lowercase()
    };
    if !matches!(proto.as_str(), "tcp" | "udp" | "sctp") {
        return Err(format!("invalid proto: {proto}"));
    }
    if ip.starts_with('[') {
        ip = split_host(&format!("{ip}:")).map_err(|e| format!("invalid IP address {ip}: {e}"))?;
    }
    if !ip.is_empty() {
        shards_cmdline::network::parse_addr(&ip).map_err(|e| format!("invalid IP address: {e}"))?;
    }
    let (start, end) = port_range(container).ok_or_else(|| format!("invalid containerPort: {container}"))?;
    if !host_port.is_empty() {
        let (host_start, host_end) =
            port_range(host_port).ok_or_else(|| format!("invalid hostPort: {host_port}"))?;
        // A host range for one container port is a range to pick from.
        if end.wrapping_sub(start) != host_end.wrapping_sub(host_start) && end != start {
            return Err(format!(
                "invalid ranges specified for container and host Ports: {container} and {host_port}"
            ));
        }
    }
    // Go counts in uint16, so 0-65535 wraps to no ports.
    let count = end.wrapping_sub(start).wrapping_add(1);
    Ok((0..count)
        .map(|i| format!("{}/{proto}", start.wrapping_add(i)))
        .collect())
}

/// moby api/types/network `ParsePortRange`'s numbers: `start[-end][/proto]`.
fn port_range(s: &str) -> Option<(u16, u16)> {
    // parsePortNumber: strconv.ParseUint(s, 10, 16), digits only.
    let number = |n: &str| {
        (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .then(|| n.parse::<u16>().ok())
            .flatten()
    };
    let range = s.split_once('/').map_or(s, |(r, _)| r);
    let (start, end) = match range.split_once('-') {
        Some((start, end)) if start != end => (number(start)?, number(end)?),
        Some((start, _)) => (number(start)?, number(start)?),
        None => (number(range)?, number(range)?),
    };
    (end >= start).then_some((start, end))
}

/// The host `net.SplitHostPort` finds in `[host]:port`, or its `AddrError`'s text.
fn split_host(hostport: &str) -> Result<String, String> {
    let err = |why: &str| format!("address {hostport}: {why}");
    let Some(last_colon) = hostport.rfind(':') else {
        return Err(err("missing port in address"));
    };
    let Some(end) = hostport.find(']') else {
        return Err(err("missing ']' in address"));
    };
    if end + 1 == hostport.len() {
        return Err(err("missing port in address"));
    }
    if end + 1 != last_colon {
        return Err(if hostport.as_bytes().get(end + 1) == Some(&b':') {
            err("too many colons in address")
        } else {
            err("missing port in address")
        });
    }
    if hostport.get(1..).is_some_and(|s| s.contains('[')) {
        return Err(err("unexpected '[' in address"));
    }
    if hostport.get(end + 1..).is_some_and(|s| s.contains(']')) {
        return Err(err("unexpected ']' in address"));
    }
    Ok(hostport.get(1..end).unwrap_or_default().to_string())
}

/// Linux's signal names, without `SIG`: moby/sys/signal v0.7.1 signal_linux.go.
const SIGNALS: [&[u8]; 34] = [
    b"ABRT", b"ALRM", b"BUS", b"CHLD", b"CLD", b"CONT", b"FPE", b"HUP", b"ILL", b"INT", b"IO", b"IOT",
    b"KILL", b"PIPE", b"POLL", b"PROF", b"PWR", b"QUIT", b"SEGV", b"STKFLT", b"STOP", b"SYS", b"TERM",
    b"TRAP", b"TSTP", b"TTIN", b"TTOU", b"URG", b"USR1", b"USR2", b"VTALRM", b"WINCH", b"XCPU", b"XFSZ",
];

/// moby/sys/signal `ParseSignal`, checking only: a nonzero number, or a signal's name,
/// any case, with or without `SIG`.
fn parse_signal(raw: &[u8]) -> Result<(), String> {
    let invalid = || format!("invalid signal: {}", text(raw));
    // strconv.Atoi: an optional sign, then decimal digits, within an int.
    if let Some(n) = std::str::from_utf8(raw).ok().and_then(|s| s.parse::<i64>().ok()) {
        return if n == 0 { Err(invalid()) } else { Ok(()) };
    }
    let upper = go::to_upper(raw);
    let name = upper.strip_prefix(b"SIG").unwrap_or(&upper);
    if SIGNALS.contains(&name) || realtime(name) {
        Ok(())
    } else {
        Err(invalid())
    }
}

/// `RTMIN`, `RTMIN+1` to `RTMIN+15`, `RTMAX-14` to `RTMAX-1`, and `RTMAX`.
fn realtime(name: &[u8]) -> bool {
    name == b"RTMIN"
        || name == b"RTMAX"
        || (1..=15).any(|n| name == format!("RTMIN+{n}").as_bytes())
        || (1..=14).any(|n| name == format!("RTMAX-{n}").as_bytes())
}
