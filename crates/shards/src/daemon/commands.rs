//! Container commands the daemon runs for its clients (`shards wait`, `rm`, `stop`,
//! `kill`), answering on the client's stdout and stderr as `docker` answers
//! (docs/design/architecture.md D27).

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shards_ipc::kind;

use super::{Daemon, KILL_GRACE, STOP_GRACE, lock};

/// The client's end: what a command prints goes there.
pub(super) struct Reply<'a>(pub &'a UnixStream);

impl Reply<'_> {
    fn out(&self, line: &str) {
        let _ = shards_ipc::send(self.0, kind::OUT, format!("{line}\n").as_bytes(), &[]);
    }

    fn err(&self, line: &str) {
        let _ = shards_ipc::send(self.0, kind::ERR, format!("{line}\n").as_bytes(), &[]);
    }
}

/// A command line that asked for something the command does not know: the Docker CLI's
/// status for a usage error.
const USAGE: u8 = 125;

/// Linux's signal names, by number (signal(7)), for `-s`.
const SIGNALS: [(&str, u32); 31] = [
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("STKFLT", 16),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];

/// A signal as `docker kill -s` takes it: a number, or a name with or without `SIG`, in
/// any case.
fn signal(given: &str) -> Option<u32> {
    if let Ok(n) = given.parse::<u32>() {
        return (1..=64).contains(&n).then_some(n);
    }
    let upper = given.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS.iter().find(|(s, _)| *s == name).map(|&(_, n)| n)
}

/// A command's flags, and the arguments after them.
struct Parsed {
    flags: Vec<(String, Option<String>)>,
    args: Vec<String>,
}

impl Parsed {
    fn has(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|(f, _)| names.contains(&f.as_str()))
    }

    fn value(&self, names: &[&str]) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| names.contains(&f.as_str()))
            .and_then(|(_, v)| v.as_deref())
    }
}

/// Splits `argv` into flags and arguments. `valued` are the flags that take a value, as
/// the next word or after `=`; `switches` the ones that take none.
fn parse(argv: &[String], valued: &[&str], switches: &[&str]) -> Result<Parsed, String> {
    let mut parsed = Parsed {
        flags: Vec::new(),
        args: Vec::new(),
    };
    let mut words = argv.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            parsed.args.extend(words.by_ref().cloned());
            break;
        }
        if !word.starts_with('-') || word == "-" {
            parsed.args.push(word.clone());
            continue;
        }
        let (flag, inline) = match word.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (word.as_str(), None),
        };
        if valued.contains(&flag) {
            let value = match inline {
                Some(v) => v,
                None => words
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("flag needs an argument: {flag}"))?,
            };
            parsed.flags.push((flag.to_string(), Some(value)));
        } else if switches.contains(&flag) && inline.is_none() {
            parsed.flags.push((flag.to_string(), None));
        } else {
            return Err(format!("unknown flag: {flag}"));
        }
    }
    Ok(parsed)
}

impl Daemon {
    /// Runs container command `argv` for a client, answering on `reply`, and returns its
    /// exit status.
    pub(super) fn command(self: &Arc<Self>, argv: &[String], reply: &Reply<'_>) -> u8 {
        let Some((name, rest)) = argv.split_first() else {
            reply.err("shards: no container command");
            return USAGE;
        };
        let run = |valued: &[&str], switches: &[&str], f: &dyn Fn(Parsed) -> u8| match parse(
            rest, valued, switches,
        ) {
            Ok(parsed) if parsed.args.is_empty() => {
                reply.err(&format!("\"shards {name}\" requires at least 1 argument."));
                USAGE
            }
            Ok(parsed) => f(parsed),
            Err(e) => {
                reply.err(&e);
                USAGE
            }
        };
        match name.as_str() {
            "wait" => run(&[], &[], &|p| self.wait(&p.args, reply)),
            "rm" => run(&[], &["-f", "--force", "-v", "--volumes"], &|p| {
                self.rm(&p.args, p.has(&["-f", "--force"]), reply)
            }),
            "stop" => run(&["-t", "--time", "--timeout", "-s", "--signal"], &[], &|p| {
                self.stop(&p, reply)
            }),
            "kill" => run(&["-s", "--signal"], &[], &|p| self.kill(&p, reply)),
            other => {
                reply.err(&format!("shards: unknown container command {other:?}"));
                USAGE
            }
        }
    }

    /// The ID of the container `reference` names: all of its ID, its name, or the start of
    /// its ID and of no other's (moby daemon/container.go, GetContainer).
    fn resolve(&self, reference: &str) -> Result<String, String> {
        let registry = lock(&self.containers);
        if !reference.is_empty() {
            if registry.get(reference).is_some() {
                return Ok(reference.to_string());
            }
            let name = reference.strip_prefix('/').unwrap_or(reference);
            if let Some(c) = registry.name_taken(name) {
                return Ok(c.id.clone());
            }
            let mut matching = registry.all().filter(|c| c.id.starts_with(reference));
            if let Some(first) = matching.next() {
                if matching.next().is_some() {
                    return Err(format!(
                        "Error response from daemon: multiple IDs found with provided prefix: {reference}"
                    ));
                }
                return Ok(first.id.clone());
            }
        }
        Err(format!(
            "Error response from daemon: No such container: {reference}"
        ))
    }

    /// Whether the container with `id` runs, as the daemon follows it.
    fn running(&self, id: &str) -> bool {
        lock(&self.runs).contains_key(id)
    }

    /// Sends Linux signal `linux` to the command of the container with `id`.
    fn signal(&self, id: &str, linux: u32) -> bool {
        lock(&self.runs)
            .get(id)
            .is_some_and(|t| shards_ipc::send(&t.socket, kind::SIGNAL, &linux.to_be_bytes(), &[]).is_ok())
    }

    /// Waits up to `limit` (or for ever) for the container with `id` to stop running.
    fn await_end(&self, id: &str, limit: Option<Duration>) -> bool {
        let deadline = limit.map(|l| Instant::now() + l);
        let mut registry = lock(&self.containers);
        loop {
            if !self.running(id) {
                return true;
            }
            registry = match deadline {
                None => self
                    .ended
                    .wait(registry)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    self.ended
                        .wait_timeout(registry, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// `shards wait`: blocks until each container stops, then prints its exit code.
    fn wait(&self, references: &[String], reply: &Reply<'_>) -> u8 {
        let mut status = 0;
        for reference in references {
            match self.resolve(reference) {
                Ok(id) => {
                    self.await_end(&id, None);
                    let code = lock(&self.containers).get(&id).and_then(|c| c.exit_code);
                    reply.out(&code.map_or(-1, i32::from).to_string());
                }
                Err(e) => {
                    reply.err(&e);
                    status = 1;
                }
            }
        }
        status
    }

    /// `shards rm [-f]`: removes containers that have stopped; with `-f`, kills the ones
    /// still running first.
    fn rm(&self, references: &[String], force: bool, reply: &Reply<'_>) -> u8 {
        let mut status = 0;
        for reference in references {
            let removed = self.resolve(reference).and_then(|id| {
                if self.running(&id) {
                    if !force {
                        let name = lock(&self.containers)
                            .get(&id)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        return Err(format!(
                            "Error response from daemon: cannot remove container \"/{name}\": container is running: stop the container before removing or force remove"
                        ));
                    }
                    self.signal(&id, 9);
                    if !self.await_end(&id, Some(KILL_GRACE)) {
                        self.kill_vm(&id);
                        self.await_end(&id, None);
                    }
                }
                lock(&self.containers)
                    .remove(&id)
                    .map_err(|e| format!("Error response from daemon: {e}"))
            });
            match removed {
                Ok(_) => reply.out(reference),
                Err(e) => {
                    reply.err(&e);
                    status = 1;
                }
            }
        }
        status
    }

    /// Ends the VM of the container with `id`, whose command did not end.
    fn kill_vm(&self, id: &str) {
        if let Some(t) = lock(&self.runs).get(id) {
            let _ = t.vm.kill(libc::SIGKILL);
        }
    }

    /// `shards stop [-t SECONDS] [-s SIGNAL]`: the signal (SIGTERM), then SIGKILL once the
    /// time (10 s) is up, as dockerd stops a container (moby daemon/stop.go). A negative
    /// time waits for ever.
    fn stop(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let time = parsed.value(&["-t", "--time", "--timeout"]);
        let grace = match time.map(str::parse::<i64>) {
            None => Some(STOP_GRACE),
            Some(Ok(seconds)) if seconds < 0 => None,
            Some(Ok(seconds)) => Some(Duration::from_secs(seconds.unsigned_abs())),
            Some(Err(_)) => {
                reply.err(&format!(
                    "invalid argument {:?} for \"-t, --timeout\" flag",
                    time.unwrap_or_default()
                ));
                return USAGE;
            }
        };
        let first = match parsed.value(&["-s", "--signal"]).map(|s| (s, signal(s))) {
            None => 15,
            Some((_, Some(n))) => n,
            Some((given, None)) => {
                reply.err(&format!("Error response from daemon: Invalid signal: {given}"));
                return 1;
            }
        };
        let mut status = 0;
        for reference in &parsed.args {
            match self.resolve(reference) {
                Ok(id) => {
                    if self.running(&id) {
                        self.signal(&id, first);
                        if !self.await_end(&id, grace) {
                            self.signal(&id, 9);
                            if !self.await_end(&id, Some(KILL_GRACE)) {
                                self.kill_vm(&id);
                                self.await_end(&id, None);
                            }
                        }
                    }
                    reply.out(reference);
                }
                Err(e) => {
                    reply.err(&e);
                    status = 1;
                }
            }
        }
        status
    }

    /// `shards kill [-s SIGNAL]`: the signal (SIGKILL) to each running container's command.
    fn kill(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let linux = match parsed.value(&["-s", "--signal"]).map(|s| (s, signal(s))) {
            None => 9,
            Some((_, Some(n))) => n,
            Some((given, None)) => {
                reply.err(&format!("Error response from daemon: Invalid signal: {given}"));
                return 1;
            }
        };
        let mut status = 0;
        for reference in &parsed.args {
            let sent = self.resolve(reference).and_then(|id| {
                if self.signal(&id, linux) {
                    Ok(())
                } else {
                    Err(format!(
                        "Error response from daemon: cannot kill container: {reference}: container {id} is not running"
                    ))
                }
            });
            match sent {
                Ok(()) => reply.out(reference),
                Err(e) => {
                    reply.err(&e);
                    status = 1;
                }
            }
        }
        status
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn signals_are_taken_as_docker_kill_takes_them() {
        for (given, n) in [
            ("KILL", 9),
            ("SIGTERM", 15),
            ("term", 15),
            ("sigusr1", 10),
            ("9", 9),
        ] {
            assert_eq!(signal(given), Some(n), "{given}");
        }
        for bad in ["", "SIG", "NOPE", "0", "65", "-1"] {
            assert_eq!(signal(bad), None, "{bad}");
        }
    }

    #[test]
    fn flags_take_values_as_the_docker_cli_does() {
        let words = |w: &[&str]| w.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let p = parse(
            &words(&["-t", "3", "--signal=USR1", "web", "api"]),
            &["-t", "--signal"],
            &[],
        )
        .unwrap();
        assert_eq!(p.value(&["-t"]), Some("3"));
        assert_eq!(p.value(&["--signal"]), Some("USR1"));
        assert_eq!(p.args, ["web", "api"]);
        assert!(parse(&words(&["-t"]), &["-t"], &[]).is_err());
        assert!(parse(&words(&["--nope", "x"]), &[], &[]).is_err());
        let p = parse(&words(&["-f", "--", "-web"]), &[], &["-f"]).unwrap();
        assert!(p.has(&["-f"]));
        assert_eq!(p.args, ["-web"]);
    }
}
