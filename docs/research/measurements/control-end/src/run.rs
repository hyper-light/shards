//! `control-end run`: the measurement (README.md). Each round runs every arm asked for, in an
//! order rotated round by round, each arm on microVMs of its own, booted for it and ended
//! after it:
//!
//! - g: a daemon run per microVM, its command arm g's endpoint, its UDP port published on the
//!   host's loopback through the run's network process (shards-net);
//! - g0: the same runs holding no endpoint (`guest idle`), arm g's baseline;
//! - h: a directly booted microVM per controller connection, PID 1 a vsock listener, and one
//!   host endpoint, a process of its own, that every connection reaches;
//! - h0: the same microVMs holding no listener and no endpoint, arm h's baseline.
//!
//! For each, the processes Shards runs for it (and the endpoint's) are sampled: with nothing
//! connected (S0, then S1 one idle window later), then for g and h with every microVM's
//! connection made (S2), one idle window later (S3), after the round trips (S4), after the
//! transfers (S5), and after every microVM's second connection, a new controller's (R1).

use std::collections::BTreeMap;
use std::io::{BufRead as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_remote::net::{Command as DriverCommand, Driver, Sender};

use crate::client::{Ask, Client, Said};
use crate::procs::{self, Proc, Usage};
use crate::proto::{self, Opts};
use crate::record::{self, Rec, Records};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    G,
    G0,
    H,
    H0,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::G => "g",
            Arm::G0 => "g0",
            Arm::H => "h",
            Arm::H0 => "h0",
        }
    }

    fn parse(text: &str) -> Result<Arm, String> {
        match text {
            "g" => Ok(Arm::G),
            "g0" => Ok(Arm::G0),
            "h" => Ok(Arm::H),
            "h0" => Ok(Arm::H0),
            other => Err(format!("arm {other:?}: g, g0, h or h0")),
        }
    }

    /// Whether the arm holds an endpoint the controller connects to.
    fn controls(self) -> bool {
        matches!(self, Arm::G | Arm::H)
    }

    fn daemon(self) -> bool {
        matches!(self, Arm::G | Arm::G0)
    }
}

#[derive(Debug)]
struct Config {
    arms: Vec<Arm>,
    vms: u32,
    rounds: u32,
    shards: PathBuf,
    guest: PathBuf,
    out: PathBuf,
    kernel: Option<PathBuf>,
    sock_dir: PathBuf,
    cpus: u32,
    memory_mib: u64,
    idle: Duration,
    pings: u32,
    ping_bytes: usize,
    bulk_bytes: u64,
    bulk_vms: u32,
    bulk_rounds: u32,
    ready: Duration,
    spin: u32,
    harness_rev: String,
}

const OPTIONS: &[&str] = &[
    "arms",
    "vms",
    "rounds",
    "shards",
    "guest",
    "out",
    "kernel",
    "sock-dir",
    "cpus",
    "memory-mib",
    "idle-s",
    "pings",
    "ping-bytes",
    "bulk-bytes",
    "bulk-vms",
    "bulk-rounds",
    "ready-s",
    "spin",
    "harness-rev",
];

impl Config {
    fn parse(args: &[String]) -> Result<Config, String> {
        let o = Opts::parse(args, OPTIONS)?;
        let arms = o
            .get("arms")
            .unwrap_or("g0,g,h0,h")
            .split(',')
            .map(Arm::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let out = absolute(Path::new(o.need("out")?))?;
        let sock_dir = match o.get("sock-dir") {
            Some(d) => absolute(Path::new(d))?,
            None => out.join("s"),
        };
        let config = Config {
            arms,
            vms: o.num("vms", 8)?,
            rounds: o.num("rounds", 3)?,
            shards: absolute(Path::new(o.need("shards")?))?,
            guest: absolute(Path::new(o.need("guest")?))?,
            out,
            kernel: o.get("kernel").map(|k| absolute(Path::new(k))).transpose()?,
            sock_dir,
            cpus: o.num("cpus", 1)?,
            memory_mib: o.num("memory-mib", 512)?,
            idle: Duration::from_secs(o.num("idle-s", 10)?),
            pings: o.num("pings", 100)?,
            ping_bytes: o.num("ping-bytes", 64)?,
            bulk_bytes: o.num("bulk-bytes", 16 << 20)?,
            bulk_vms: o.num("bulk-vms", 1)?,
            bulk_rounds: o.num("bulk-rounds", 3)?,
            ready: Duration::from_secs(o.num("ready-s", 120)?),
            spin: o.num("spin", 0)?,
            harness_rev: o.get("harness-rev").unwrap_or("unknown").to_string(),
        };
        if config.vms == 0 || config.rounds == 0 || config.arms.is_empty() {
            return Err("--vms, --rounds and --arms need at least one".into());
        }
        if config.ping_bytes == 0 {
            return Err("--ping-bytes needs at least one byte, which comes back".into());
        }
        // sockaddr_un holds 104 bytes on macOS, 108 on Linux (sys/un.h): the longest socket
        // name the VM binds, with its NUL.
        let longest = config
            .sock_dir
            .join(crate::endpoint::sock_name(config.vms))
            .as_os_str()
            .len();
        if longest >= 104 {
            return Err(format!(
                "--sock-dir {}: its sockets' names would take {longest} bytes, past the 103 a socket's address holds",
                config.sock_dir.display()
            ));
        }
        Ok(config)
    }
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|d| d.join(path))
        .map_err(|e| format!("the working directory: {e}"))
}

pub fn main(args: &[String]) -> Result<(), String> {
    let config = Config::parse(args)?;
    std::fs::create_dir_all(config.out.join("logs")).map_err(|e| format!("{}: {e}", config.out.display()))?;
    std::fs::create_dir_all(&config.sock_dir).map_err(|e| format!("{}: {e}", config.sock_dir.display()))?;
    procs::raise_descriptor_limit();
    let mut records = Records::create(&config.out.join("records.jsonl"))?;
    let docker_config = config.out.join("docker-config");
    std::fs::create_dir_all(&docker_config).map_err(|e| format!("{}: {e}", docker_config.display()))?;
    std::fs::write(docker_config.join("config.json"), "{}").map_err(|e| e.to_string())?;
    let home = config.out.join("home");
    let shards = Shards {
        bin: config.shards.clone(),
        home: home.clone(),
        docker_config,
    };
    let version_line = shards.output(&["--version"]).unwrap_or_default();
    let version_json = shards.output(&["version", "--format", "{{json .}}"]).unwrap_or_default();
    let commit = json_field(&version_json, "GitCommit").unwrap_or_default();
    let mut host = Rec::new("host");
    let described = procs::host();
    for (k, v) in &described {
        host = host.s(k, v);
    }
    records.write(
        host.s("shards", &version_line)
            .s("shards_commit", &commit)
            .s("harness_rev", &config.harness_rev)
            .s("arms", &config.arms.iter().map(|a| a.name()).collect::<Vec<_>>().join(","))
            .u("vms", u64::from(config.vms))
            .u("rounds", u64::from(config.rounds))
            .u("cpus", u64::from(config.cpus))
            .u("memory_mib", config.memory_mib)
            .u("idle_s", config.idle.as_secs())
            .u("pings", u64::from(config.pings))
            .u("ping_bytes", config.ping_bytes as u64)
            .u("bulk_bytes", config.bulk_bytes)
            .u("bulk_vms", u64::from(config.bulk_vms))
            .u("bulk_rounds", u64::from(config.bulk_rounds))
            .u("spin", u64::from(config.spin))
            .s("mem", procs::MEM)
            .s("alt", procs::ALT),
    );
    let identity = shards_remote::identity::Identity::generate().map_err(|e| e.to_string())?;
    let mut daemon = None;
    let needs_daemon = config.arms.iter().any(|a| a.daemon()) || config.kernel.is_none();
    let mut image = String::new();
    if needs_daemon {
        daemon = Some(Daemon::start(&shards, &config.out)?);
        image = load_image(&shards, &config)?;
    }
    let kernel = match &config.kernel {
        Some(k) => k.clone(),
        None => pinned_kernel(&shards, &version_json)?,
    };
    let load = (config.spin > 0)
        .then(|| spin(config.spin))
        .transpose()?
        .map(Reaped);
    let mut context = Context {
        config: &config,
        shards: &shards,
        image,
        kernel,
        identity: &identity,
        daemon: daemon.as_ref().map(Daemon::pid),
        records: &mut records,
    };
    for round in 0..config.rounds {
        let n = config.arms.len();
        for i in 0..n {
            let Some(&arm) = config.arms.get((i + round as usize) % n) else {
                continue;
            };
            if let Err(e) = context.arm(arm, round) {
                context.records.write(
                    Rec::new("failed")
                        .s("arm", arm.name())
                        .u("round", u64::from(round))
                        .s("what", "arm")
                        .s("why", &e),
                );
                context.records.sample("arm failures", arm.name(), 1.0);
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "control-end: arm {} round {round}: {e}", arm.name());
            }
        }
    }
    drop(load);
    if let Some(daemon) = daemon.take() {
        daemon.stop(&shards);
    }
    let summary = summarize(&records, &described, &version_line, &commit, &config);
    let _ = std::fs::write(config.out.join("summary.txt"), &summary);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(summary.as_bytes());
    Ok(())
}

/// `"key":"value"` in a JSON text: the first, a string.
fn json_field(text: &str, key: &str) -> Option<String> {
    let at = format!("\"{key}\":\"");
    let (_, rest) = text.split_once(&at)?;
    rest.split_once('"').map(|(v, _)| v.to_string())
}

/// `shards`, run for this measurement's home, with a docker config of its own that names no
/// credential helper and publishing to no local image store, as the E2E tests run it.
#[derive(Debug)]
struct Shards {
    bin: PathBuf,
    home: PathBuf,
    docker_config: PathBuf,
}

impl Shards {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("SHARDS_HOME", &self.home)
            .env("SHARDS_LOCAL_STORE", "none")
            .env("DOCKER_CONFIG", &self.docker_config)
            .env_remove("DOCKER_AUTH_CONFIG")
            .env_remove("SHARDS_DEFAULT_PLATFORM")
            .env_remove("DOCKER_DEFAULT_PLATFORM")
            .stdin(Stdio::null());
        c
    }

    /// Runs `shards ARGS`; its stdout, or what it said where it failed.
    fn output(&self, args: &[&str]) -> Result<String, String> {
        let out = self
            .command()
            .args(args)
            .output()
            .map_err(|e| format!("{}: {e}", self.bin.display()))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(format!(
                "shards {}: {}{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// A child ended with this measurement however it ends.
#[derive(Debug)]
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The home's daemon, in the foreground as this process's child, so that its processes are
/// known by their parent; no warm pool, so that each run's microVM is its own
/// (`SHARDS_POOL=0`, daemon.rs).
#[derive(Debug)]
struct Daemon(Reaped);

impl Daemon {
    fn start(shards: &Shards, out: &Path) -> Result<Daemon, String> {
        let log = std::fs::File::create(out.join("logs").join("daemon.out")).map_err(|e| e.to_string())?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let child = shards
            .command()
            .arg("daemon")
            .env("SHARDS_POOL", "0")
            .stdout(log)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("{} daemon: {e}", shards.bin.display()))?;
        let mut daemon = Daemon(Reaped(child));
        // Its clients find it once its pid file names it and its request slots are made
        // (daemon.rs `serve`, D185): a client that finds them waits for its slot. One that
        // ends at once found another daemon serving the home.
        let me = daemon.pid().to_string();
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            let named = std::fs::read_to_string(shards.home.join("daemon.pid")).unwrap_or_default();
            if named.split_whitespace().next() == Some(me.as_str()) && shards.home.join("requests").is_dir() {
                break;
            }
            if let Ok(Some(status)) = daemon.0.0.try_wait() {
                return Err(format!(
                    "the daemon ended at once ({status}): another serves {}, or see logs/daemon.out",
                    shards.home.display()
                ));
            }
            if Instant::now() > until {
                return Err("the daemon named itself in no pid file in 60 s".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        daemon.wait_serving(shards)?;
        Ok(daemon)
    }

    /// Until `shards ps` answers from this daemon.
    fn wait_serving(&mut self, shards: &Shards) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if shards.output(&["ps", "-q"]).is_ok() {
                return Ok(());
            }
            if Instant::now() > until {
                return Err("the daemon answered no `ps` in 60 s".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn pid(&self) -> u32 {
        self.0.0.id()
    }

    fn stop(self, shards: &Shards) {
        let _ = shards.output(&["daemon", "stop"]);
        drop(self);
    }
}

/// Loads arm g's image into the home: the guest's program as /control-end. Its name carries
/// the program's digest, so a home loads each build once. A first run then boots its
/// template, and the home's pinned kernel downloads.
fn load_image(shards: &Shards, config: &Config) -> Result<String, String> {
    use sha2::Digest as _;
    let guest = std::fs::read(&config.guest).map_err(|e| format!("{}: {e}", config.guest.display()))?;
    let digest = proto::hex(&sha2::Sha256::digest(&guest));
    let name = format!("control-end:{}", digest.get(..12).unwrap_or("guest"));
    let archive = crate::image::archive(&guest, &name)?;
    let path = config.out.join("image.tar");
    std::fs::write(&path, archive).map_err(|e| format!("{}: {e}", path.display()))?;
    let path_text = path.to_string_lossy().into_owned();
    shards.output(&["load", "-i", &path_text])?;
    shards.output(&["run", "--rm", &name, "guest", "check"])?;
    Ok(name)
}

/// The kernel `shards` pins for this host, as its first run stored it in the home
/// (guest.rs, `sha256-<digest>` in the home's `guest` directory).
fn pinned_kernel(shards: &Shards, version: &str) -> Result<PathBuf, String> {
    let (_, kernel) = version
        .split_once("\"Guest kernel\"")
        .ok_or("`shards version` names no guest kernel: give --kernel")?;
    let sha = json_field(kernel, "Sha256").ok_or("`shards version` gives no kernel digest: give --kernel")?;
    let path = shards.home.join("guest").join(format!("sha256-{sha}"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{}: no kernel there yet: give --kernel", path.display()))
    }
}

/// `control-end spin N`: N threads that never wait, the load a busy host gives.
fn spin(threads: u32) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Command::new(exe)
        .args(["spin", &threads.to_string()])
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("the spinning load: {e}"))
}

pub fn spin_main(args: &[String]) -> Result<(), String> {
    let threads: u32 = args.first().and_then(|n| n.parse().ok()).ok_or("spin N")?;
    let mut handles = Vec::new();
    for _ in 0..threads {
        let handle = std::thread::Builder::new()
            .name("spin".into())
            .spawn(|| {
                let mut x = 0u64;
                loop {
                    x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
                }
            })
            .map_err(|e| e.to_string())?;
        handles.push(handle);
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

struct Context<'a> {
    config: &'a Config,
    shards: &'a Shards,
    image: String,
    kernel: PathBuf,
    identity: &'a shards_remote::identity::Identity,
    daemon: Option<u32>,
    records: &'a mut Records,
}

/// The processes an arm's microVMs are, at an instant; and those whose counts this process
/// may not read, which no sum includes.
#[derive(Debug)]
struct Snap {
    at: Instant,
    procs: Vec<(Proc, Usage)>,
    unread: Vec<Proc>,
}

impl Snap {
    fn take(roots: &[u32]) -> Snap {
        let table = procs::table().unwrap_or_default();
        let mut procs = Vec::new();
        let mut unread = Vec::new();
        for p in procs::tree(&table, roots) {
            match procs::usage(p.pid) {
                Some(u) => procs.push((p, u)),
                None => unread.push(p),
            }
        }
        Snap {
            at: Instant::now(),
            procs,
            unread,
        }
    }

    /// Memory held, by class and in all.
    fn memory(&self) -> BTreeMap<&'static str, u64> {
        let mut out = BTreeMap::new();
        for (p, u) in &self.procs {
            let class = procs::class(&p.name);
            *out.entry(class).or_insert(0u64) += u.mem;
            *out.entry("all").or_insert(0u64) += u.mem;
        }
        out
    }

    /// CPU used since `before`, by class and in all: each process's present in both.
    fn cpu_since(&self, before: &Snap) -> BTreeMap<&'static str, u64> {
        let mut out = BTreeMap::new();
        for (p, u) in &self.procs {
            let Some((_, b)) = before.procs.iter().find(|(q, _)| q.pid == p.pid) else {
                continue;
            };
            let used = u.cpu_ns.saturating_sub(b.cpu_ns);
            *out.entry(procs::class(&p.name)).or_insert(0u64) += used;
            *out.entry("all").or_insert(0u64) += used;
        }
        out
    }
}

/// An arm's microVMs, ended as this goes.
#[derive(Debug)]
struct Fleet<'a> {
    shards: &'a Shards,
    names: Vec<String>,
    children: Vec<Reaped>,
    endpoint: Option<Reaped>,
    roots: Vec<u32>,
    /// Where the controller connects for each microVM, and the key it pins.
    targets: Vec<(SocketAddr, [u8; 32])>,
}

impl Drop for Fleet<'_> {
    fn drop(&mut self) {
        self.endpoint = None;
        self.children.clear();
        for names in self.names.chunks(32) {
            let mut args = vec!["rm", "-f"];
            args.extend(names.iter().map(String::as_str));
            let _ = self.shards.output(&args);
        }
    }
}

impl<'a> Context<'a> {
    fn arm(&mut self, arm: Arm, round: u32) -> Result<(), String> {
        let config = self.config;
        let since = procs::SystemTimes::now();
        let base = Rec::new("arm").s("arm", arm.name()).u("round", u64::from(round));
        self.records.write(base.s("at", "start").s("load", &procs::load(None)));
        let booted = Instant::now();
        let fleet = if arm.daemon() {
            self.daemon_fleet(arm, round)?
        } else {
            self.direct_fleet(arm, round)?
        };
        let boot_s = booted.elapsed().as_secs_f64();
        let n = f64::from(config.vms);
        let s0 = Snap::take(&fleet.roots);
        self.snapshot(arm, round, "S0", &s0, &s0);
        std::thread::sleep(config.idle);
        let s1 = Snap::take(&fleet.roots);
        self.snapshot(arm, round, "S1", &s1, &s0);
        let idle = per_vm_cpu_pct(&s1, &s0, n);
        let mut end = Rec::new("arm")
            .s("arm", arm.name())
            .u("round", u64::from(round))
            .s("at", "end")
            .f("boot_s", boot_s)
            .u("processes", s1.procs.len() as u64)
            .f("idle_cpu_pct_per_vm", idle);
        self.records.sample("idle CPU per VM, nothing connected (% of a CPU)", arm.name(), idle);
        let mem = s1.memory();
        for (class, bytes) in &mem {
            let kib = *bytes as f64 / 1024.0 / n;
            end = end.f(&format!("mem_kib_per_vm_unconnected_{class}"), kib);
            if *class == "all" {
                self.records
                    .sample("memory per VM, nothing connected (KiB)", arm.name(), kib);
            }
        }
        if !arm.controls() {
            if let Some(bytes) = mem.get("all") {
                self.records
                    .sample("memory per VM, idle (KiB)", arm.name(), *bytes as f64 / 1024.0 / n);
            }
        } else {
            self.control(arm, round, &fleet, &s0, &s1, &mut end)?;
        }
        self.records.write(end.s("load", &procs::load(since)));
        drop(fleet);
        Ok(())
    }

    fn snapshot(&mut self, arm: Arm, round: u32, at: &str, snap: &Snap, first: &Snap) {
        let t = snap.at.saturating_duration_since(first.at).as_secs_f64() * 1e3;
        for (p, u) in &snap.procs {
            self.records.write(
                Rec::new("process")
                    .s("arm", arm.name())
                    .u("round", u64::from(round))
                    .s("at", at)
                    .f("t_ms", t)
                    .u("pid", u64::from(p.pid))
                    .s("name", &p.name)
                    .s("class", procs::class(&p.name))
                    .u("cpu_ns", u.cpu_ns)
                    .u("mem", u.mem)
                    .u("alt", u.alt),
            );
        }
        for p in &snap.unread {
            self.records.write(
                Rec::new("process")
                    .s("arm", arm.name())
                    .u("round", u64::from(round))
                    .s("at", at)
                    .f("t_ms", t)
                    .u("pid", u64::from(p.pid))
                    .s("name", &p.name)
                    .s("class", procs::class(&p.name))
                    .s("unread", "its counts could not be read"),
            );
            self.records.sample("processes unread", arm.name(), 1.0);
        }
    }

    /// Arm g's and g0's microVMs: daemon runs of the image.
    fn daemon_fleet(&mut self, arm: Arm, round: u32) -> Result<Fleet<'a>, String> {
        let config = self.config;
        let tag = format!("ce{}-{round}-{}", std::process::id(), arm.name());
        let mut fleet = Fleet {
            shards: self.shards,
            names: Vec::new(),
            children: Vec::new(),
            endpoint: None,
            roots: Vec::new(),
            targets: Vec::new(),
        };
        let controller = proto::hex(&self.identity.fingerprint());
        let cpus = config.cpus.to_string();
        let memory = format!("{}m", config.memory_mib);
        let published = format!("127.0.0.1::{}/udp", proto::QUIC_PORT);
        for i in 0..config.vms {
            let name = format!("{tag}-{i}");
            let mut args = vec!["run", "-d", "--name", &name, "--cpus", &cpus, "--memory", &memory];
            if arm == Arm::G {
                args.extend(["-p", &published]);
            }
            args.push(&self.image);
            match arm {
                Arm::G => args.extend(["guest", "quic", "--controller", &controller]),
                _ => args.extend(["guest", "idle"]),
            }
            fleet.names.push(name.clone());
            self.shards.output(&args)?;
        }
        let until = Instant::now() + config.ready;
        for (i, name) in fleet.names.iter().enumerate() {
            let said = loop {
                let logs = self.shards.output(&["logs", name]).unwrap_or_default();
                if let Some(line) = logs.lines().find(|l| l.starts_with("control-end ")) {
                    break line.to_string();
                }
                if Instant::now() > until {
                    return Err(format!("{name} said nothing in {:?}", config.ready));
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            if arm == Arm::G {
                let fp = said
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("fingerprint="))
                    .ok_or_else(|| format!("{name}: {said:?}"))?;
                let fp = proto::fingerprint(fp)?;
                let port = self.shards.output(&["port", name, &format!("{}/udp", proto::QUIC_PORT)])?;
                let addr: SocketAddr = port
                    .lines()
                    .find_map(|l| l.trim().parse().ok())
                    .ok_or_else(|| format!("{name}: port {port:?}"))?;
                fleet.targets.push((addr, fp));
                self.records.write(
                    Rec::new("vm")
                        .s("arm", arm.name())
                        .u("round", u64::from(round))
                        .u("vm", i as u64)
                        .s("name", name)
                        .s("addr", &addr.to_string())
                        .s("fingerprint", &proto::hex(&fp)),
                );
            }
        }
        // The daemon, and through its spawner its runs' VM and network processes.
        fleet.roots = self.daemon.into_iter().collect();
        Ok(fleet)
    }

    /// Arm h's and h0's microVMs, each booted directly with this program as PID 1, and arm
    /// h's endpoint.
    fn direct_fleet(&mut self, arm: Arm, round: u32) -> Result<Fleet<'a>, String> {
        let config = self.config;
        let mut fleet = Fleet {
            shards: self.shards,
            names: Vec::new(),
            children: Vec::new(),
            endpoint: None,
            roots: Vec::new(),
            targets: Vec::new(),
        };
        let mode = if arm == Arm::H { "vsock" } else { "idle" };
        let cmdline = format!("console=ttyS0 quiet control_end={mode}");
        let mut logs = Vec::new();
        for i in 0..config.vms {
            let sock = config.sock_dir.join(crate::endpoint::sock_name(i));
            let _ = std::fs::remove_file(&sock);
            let log_path = config
                .out
                .join("logs")
                .join(format!("r{round}-{}-v{i}.console", arm.name()));
            let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
            let err = log.try_clone().map_err(|e| e.to_string())?;
            let child = self
                .shards
                .command()
                .args(["run", "--kernel"])
                .arg(&self.kernel)
                .arg("--init")
                .arg(&config.guest)
                .args(["--cpus", &config.cpus.to_string()])
                .args(["--memory", &config.memory_mib.to_string()])
                .arg("--vsock")
                .arg(&sock)
                .args(["--cmdline", &cmdline])
                .stdout(log)
                .stderr(err)
                .spawn()
                .map_err(|e| format!("{} run --kernel: {e}", self.shards.bin.display()))?;
            fleet.roots.push(child.id());
            fleet.children.push(Reaped(child));
            logs.push(log_path);
        }
        let until = Instant::now() + config.ready;
        for (i, log) in logs.iter().enumerate() {
            loop {
                let text = std::fs::read_to_string(log).unwrap_or_default();
                if text.contains("control-end ready") || text.contains("control-end idle") {
                    break;
                }
                if let Some(Reaped(child)) = fleet.children.get_mut(i)
                    && let Ok(Some(status)) = child.try_wait()
                {
                    return Err(format!("microVM {i} ended: {status}; see {}", log.display()));
                }
                if Instant::now() > until {
                    return Err(format!("microVM {i} said nothing in {:?}; see {}", config.ready, log.display()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if arm == Arm::H {
            let (endpoint, addr, fp) = self.endpoint(round)?;
            fleet.roots.push(endpoint.0.id());
            fleet.endpoint = Some(endpoint);
            fleet.targets = (0..config.vms).map(|_| (addr, fp)).collect();
        }
        Ok(fleet)
    }

    /// Arm h's endpoint: this program, a process of its own; where it listens and its key.
    fn endpoint(&self, round: u32) -> Result<(Reaped, SocketAddr, [u8; 32]), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let log = std::fs::File::create(self.config.out.join("logs").join(format!("r{round}-h-endpoint.err")))
            .map_err(|e| e.to_string())?;
        let mut child = Command::new(exe)
            .arg("endpoint")
            .args(["--listen", "127.0.0.1:0"])
            .args(["--controller", &proto::hex(&self.identity.fingerprint())])
            .arg("--sock-dir")
            .arg(&self.config.sock_dir)
            .args(["--vms", &self.config.vms.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("the endpoint: {e}"))?;
        let stdout = child.stdout.take();
        let reaped = Reaped(child);
        let mut line = String::new();
        if let Some(out) = stdout {
            let _ = std::io::BufReader::new(out).read_line(&mut line);
        }
        let word = |key: &str| {
            line.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .map(str::to_string)
        };
        let addr: SocketAddr = word("listening=")
            .and_then(|a| a.parse().ok())
            .ok_or_else(|| format!("the endpoint said {line:?}"))?;
        let fp = proto::fingerprint(&word("fingerprint=").unwrap_or_default())?;
        Ok((reaped, addr, fp))
    }

    /// The controlled arms' phases: connect, idle, round trips, transfers, the guests' own
    /// counts.
    fn control(
        &mut self,
        arm: Arm,
        round: u32,
        fleet: &Fleet<'_>,
        s0: &Snap,
        s1: &Snap,
        end: &mut Rec,
    ) -> Result<(), String> {
        let config = self.config;
        let a = arm.name();
        let n = f64::from(config.vms);
        let step = Duration::from_secs(60).max(config.ready);
        // Every connection at once: the handshakes at density, each microVM's first.
        let clients = Clients::start(&fleet.targets, self.identity, config.ping_bytes)?;
        let s2 = self.connect_all(arm, round, "first", &clients, fleet, (s0, s1), end)?;
        self.snapshot(arm, round, "S2", &s2, s0);
        std::thread::sleep(config.idle);
        let s3 = Snap::take(&fleet.roots);
        self.snapshot(arm, round, "S3", &s3, s0);
        let connected_idle = per_vm_cpu_pct(&s3, &s2, n);
        *end = std::mem::take(end).f("connected_idle_cpu_pct_per_vm", connected_idle);
        self.records
            .sample("idle CPU per VM, connected (% of a CPU)", a, connected_idle);
        for (class, bytes) in s3.memory() {
            let kib = bytes as f64 / 1024.0 / n;
            *end = std::mem::take(end).f(&format!("mem_kib_per_vm_{class}"), kib);
            if class == "all" {
                self.records.sample("memory per VM, idle (KiB)", a, kib);
            }
        }
        clients.ask(
            0..config.vms,
            Ask::Ping {
                rounds: config.pings,
                size: config.ping_bytes,
            },
        );
        for said in clients.collect(config.vms, step.saturating_mul(config.pings.max(1))) {
            match said {
                Said::Pinged { vm, rtt_ns } => {
                    for rtt in &rtt_ns {
                        self.records.sample("round trip (µs)", a, *rtt as f64 / 1e3);
                    }
                    self.records.write(
                        Rec::new("rtt")
                            .s("arm", a)
                            .u("round", u64::from(round))
                            .u("vm", u64::from(vm))
                            .u("bytes", config.ping_bytes as u64)
                            .list("rtt_ns", &rtt_ns),
                    );
                }
                other => self.failed(arm, round, other),
            }
        }
        let s4 = Snap::take(&fleet.roots);
        self.snapshot(arm, round, "S4", &s4, s0);
        let movers = config.bulk_vms.min(config.vms);
        for _ in 0..config.bulk_rounds {
            for ask in [Ask::Upload(config.bulk_bytes), Ask::Download(config.bulk_bytes)] {
                clients.ask(0..movers, ask);
                let started = Instant::now();
                let mut moved = 0u64;
                for said in clients.collect(movers, step.saturating_mul(10)) {
                    match said {
                        Said::Moved { vm, upload, bytes, ns } => {
                            let mib_s = bytes as f64 / (1 << 20) as f64 / (ns as f64 / 1e9).max(1e-9);
                            let dir = if upload { "upload" } else { "download" };
                            self.records.sample(&format!("{dir}, each stream (MiB/s)"), a, mib_s);
                            self.records.write(
                                Rec::new("bulk")
                                    .s("arm", a)
                                    .u("round", u64::from(round))
                                    .u("vm", u64::from(vm))
                                    .s("dir", dir)
                                    .u("bytes", bytes)
                                    .u("ns", ns),
                            );
                            moved = moved.saturating_add(bytes);
                        }
                        other => self.failed(arm, round, other),
                    }
                }
                let secs = started.elapsed().as_secs_f64();
                let dir = if matches!(ask, Ask::Upload(_)) { "upload" } else { "download" };
                self.records.sample(
                    &format!("{dir}, all {movers} streams together (MiB/s)"),
                    a,
                    moved as f64 / (1 << 20) as f64 / secs.max(1e-9),
                );
            }
        }
        let s5 = Snap::take(&fleet.roots);
        self.snapshot(arm, round, "S5", &s5, s0);
        clients.ask(0..config.vms, Ask::Stats);
        for said in clients.collect(config.vms, step) {
            match said {
                Said::Stats { vm, line } => {
                    let field = |k: &str| {
                        line.split_whitespace()
                            .find_map(|w| w.strip_prefix(k))
                            .and_then(|v| v.parse::<f64>().ok())
                    };
                    if let Some(v) = field("rss_anon_kib=") {
                        self.records.sample("guest program RssAnon (KiB)", a, v);
                    }
                    if let Some(v) = field("pss_kib=") {
                        self.records.sample("guest program Pss (KiB)", a, v);
                    }
                    if let Some(v) = field("handshake_cpu_us=") {
                        self.records.sample("guest handshake CPU, its process (µs)", a, v);
                    }
                    self.records.write(
                        Rec::new("stats")
                            .s("arm", a)
                            .u("round", u64::from(round))
                            .u("vm", u64::from(vm))
                            .s("line", &line),
                    );
                }
                other => self.failed(arm, round, other),
            }
        }
        clients.close();
        // Every microVM's second connection, from a controller started again: its endpoint
        // has served one already, its code and the provider's tables in memory.
        let again = Clients::start(&fleet.targets, self.identity, config.ping_bytes)?;
        let r1 = self.connect_all(arm, round, "reconnect", &again, fleet, (s0, s1), end)?;
        self.snapshot(arm, round, "R1", &r1, s0);
        again.close();
        Ok(())
    }

    /// Asks every connection of `clients` to connect at once and records each one's handshake
    /// and first round trip as `label`'s; the host's CPU per microVM over the window past what
    /// the same window costs idle, by `idle`'s two snapshots. The snapshot after.
    #[allow(clippy::too_many_arguments)]
    fn connect_all(
        &mut self,
        arm: Arm,
        round: u32,
        label: &str,
        clients: &Clients,
        fleet: &Fleet<'_>,
        idle: (&Snap, &Snap),
        end: &mut Rec,
    ) -> Result<Snap, String> {
        let config = self.config;
        let a = arm.name();
        let n = f64::from(config.vms);
        let before = Snap::take(&fleet.roots);
        clients.ask(0..config.vms, Ask::Connect);
        for said in clients.collect(config.vms, Duration::from_secs(60).max(config.ready)) {
            match said {
                Said::Connected {
                    vm,
                    handshake_ns,
                    cpu_ns,
                    first_ns,
                } => {
                    self.records.write(
                        Rec::new("handshake")
                            .s("arm", a)
                            .u("round", u64::from(round))
                            .s("which", label)
                            .u("vm", u64::from(vm))
                            .u("handshake_ns", handshake_ns)
                            .u("client_cpu_ns", cpu_ns)
                            .u("first_ns", first_ns),
                    );
                    self.records
                        .sample(&format!("handshake, {label} (ms)"), a, handshake_ns as f64 / 1e6);
                    self.records
                        .sample(&format!("client handshake CPU, {label} (µs)"), a, cpu_ns as f64 / 1e3);
                    self.records.sample(
                        &format!("connect to first round trip, {label} (ms)"),
                        a,
                        first_ns as f64 / 1e6,
                    );
                }
                other => self.failed(arm, round, other),
            }
        }
        let after = Snap::take(&fleet.roots);
        let (s0, s1) = idle;
        let window = after.at.saturating_duration_since(before.at).as_secs_f64();
        let idle_window = s1.at.saturating_duration_since(s0.at).as_secs_f64();
        let idle_cpu = s1.cpu_since(s0);
        for (class, used) in after.cpu_since(&before) {
            let idle_rate = idle_cpu.get(class).copied().unwrap_or(0) as f64 / idle_window.max(1e-9);
            let us = (used as f64 - idle_rate * window) / n / 1e3;
            *end = std::mem::take(end).f(&format!("handshake_cpu_us_per_vm_{label}_{class}"), us);
            self.records
                .sample(&format!("handshake CPU per VM, {label}, host {class} (µs)"), a, us);
        }
        Ok(after)
    }

    fn failed(&mut self, arm: Arm, round: u32, said: Said) {
        let (vm, what, why) = match said {
            Said::Failed { vm, what, why } => (vm, what, why),
            other => (u32::MAX, "unexpected", format!("{other:?}")),
        };
        self.records.sample("failures", arm.name(), 1.0);
        self.records.write(
            Rec::new("failed")
                .s("arm", arm.name())
                .u("round", u64::from(round))
                .u("vm", u64::from(vm))
                .s("what", what)
                .s("why", &why),
        );
    }
}

/// CPU per microVM between two snapshots, as a percentage of one CPU.
fn per_vm_cpu_pct(after: &Snap, before: &Snap, n: f64) -> f64 {
    let secs = after.at.saturating_duration_since(before.at).as_secs_f64();
    let used = after.cpu_since(before).get("all").copied().unwrap_or(0) as f64;
    used / 1e9 / secs.max(1e-9) / n * 100.0
}

/// The controller's connections, a driver and thread each.
pub(crate) struct Clients {
    senders: Vec<Sender<Ask>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    said: mpsc::Receiver<Said>,
}

impl Clients {
    pub(crate) fn start(
        targets: &[(SocketAddr, [u8; 32])],
        identity: &shards_remote::identity::Identity,
        first_size: usize,
    ) -> Result<Clients, String> {
        let (tx, said) = mpsc::channel();
        let mut clients = Clients {
            senders: Vec::new(),
            threads: Vec::new(),
            said,
        };
        for (vm, (addr, fp)) in targets.iter().enumerate() {
            let config = shards_remote::tls::client(identity, *fp).map_err(|e| e.to_string())?;
            let vm = u32::try_from(vm).map_err(|e| e.to_string())?;
            let local: SocketAddr = if addr.is_ipv4() {
                ([127, 0, 0, 1], 0).into()
            } else {
                (std::net::Ipv6Addr::LOCALHOST, 0).into()
            };
            let client = Client::new(vm, *addr, config, tx.clone(), first_size);
            let (mut driver, sender) = Driver::bind(local, None, client).map_err(|e| e.to_string())?;
            let thread = std::thread::Builder::new()
                .name(format!("client {vm}"))
                .spawn(move || {
                    let _ = driver.run();
                })
                .map_err(|e| e.to_string())?;
            clients.senders.push(sender);
            clients.threads.push(thread);
        }
        Ok(clients)
    }

    pub(crate) fn ask(&self, vms: std::ops::Range<u32>, ask: Ask) {
        for vm in vms {
            if let Some(s) = self.senders.get(vm as usize) {
                let _ = s.send(DriverCommand::Handler(ask));
            }
        }
    }

    /// `n` answers, or as many as come within `wait`; those that do not come are said as
    /// failures.
    pub(crate) fn collect(&self, n: u32, wait: Duration) -> Vec<Said> {
        let until = Instant::now() + wait;
        let mut out = Vec::new();
        while out.len() < n as usize {
            let left = until.saturating_duration_since(Instant::now());
            match self.said.recv_timeout(left) {
                Ok(said) => out.push(said),
                Err(_) => {
                    out.push(Said::Failed {
                        vm: u32::MAX,
                        what: "timeout",
                        why: format!("{} of {n} answered within {wait:?}", out.len()),
                    });
                    break;
                }
            }
        }
        out
    }

    pub(crate) fn close(self) {
        for s in &self.senders {
            let _ = s.send(DriverCommand::Handler(Ask::Close));
        }
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// The summary: each metric's distribution per arm, then each arm's cost over its baseline.
fn summarize(records: &Records, host: &[(String, String)], version: &str, commit: &str, config: &Config) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# control-end: where agent control terminates (README.md)");
    for (k, v) in host {
        let _ = writeln!(out, "{k}: {v}");
    }
    let _ = writeln!(out, "shards: {version} (commit {commit})");
    let _ = writeln!(out, "harness: {}", config.harness_rev);
    let _ = writeln!(
        out,
        "vms {} | rounds {} | vCPUs {} | memory {} MiB | idle {:?} | pings {} x {} B | bulk {} B x {} streams x {} | spin {}",
        config.vms,
        config.rounds,
        config.cpus,
        config.memory_mib,
        config.idle,
        config.pings,
        config.ping_bytes,
        config.bulk_bytes,
        config.bulk_vms,
        config.bulk_rounds,
        config.spin
    );
    let _ = writeln!(
        out,
        "memory is {} ({} in records.jsonl); load per arm in records.jsonl\n",
        procs::MEM,
        procs::ALT
    );
    let _ = writeln!(out, "| metric | arm | n | p50 | p90 | p99 | max |");
    let _ = writeln!(out, "|---|---|---|---|---|---|---|");
    for (metric, arm) in records.metrics() {
        if let Some(d) = record::dist(records.samples(&metric, &arm)) {
            let _ = writeln!(
                out,
                "| {metric} | {arm} | {} | {:.1} | {:.1} | {:.1} | {:.1} |",
                d.n, d.p50, d.p90, d.p99, d.max
            );
        }
    }
    let _ = writeln!(out, "\n## Termination's cost per VM over its baseline (p50 of each arm's runs)\n");
    let _ = writeln!(out, "| metric | g - g0 | h - h0 |");
    let _ = writeln!(out, "|---|---|---|");
    let p50 = |metric: &str, arm: &str| record::dist(records.samples(metric, arm)).map(|d| d.p50);
    let delta = |metric: &str, a: &str, b: &str, base: &str| match (p50(metric, a), p50(base, b)) {
        (Some(x), Some(y)) => format!("{:.1}", x - y),
        _ => "not run".into(),
    };
    let mem = "memory per VM, idle (KiB)";
    let idle = "idle CPU per VM, nothing connected (% of a CPU)";
    let connected = "idle CPU per VM, connected (% of a CPU)";
    let _ = writeln!(
        out,
        "| memory per VM, idle, connected (KiB) | {} | {} |",
        delta(mem, "g", "g0", mem),
        delta(mem, "h", "h0", mem)
    );
    let _ = writeln!(
        out,
        "| idle CPU per VM, connected (% of a CPU) | {} | {} |",
        delta(connected, "g", "g0", idle),
        delta(connected, "h", "h0", idle)
    );
    out
}
