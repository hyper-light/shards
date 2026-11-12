//! On-demand OTLP artifacts on the owner's existing output descriptor.

use std::io::{self, Write as _};

use shards_ipc::fd::{AsFd as _, OwnedFd};

use super::commands::{Asker, Reply};
use super::health::Wait;
use super::{Daemon, Disk, RunState, TAKE_TIMEOUT, lock};

/// Why a run's metrics were not read, as `read metrics` says it.
enum Failure {
    Resolve(String),
    /// The run named does not run.
    NotRunning {
        given: String,
    },
    /// The microVM its metrics are read in is paused, running none of its guest: its own,
    /// or that of the provider whose microVM it joined (D119), named as it is shown.
    Paused {
        given: String,
        provider: Option<String>,
    },
    /// Its microVM's process ran nothing for as long as the read's wait judges it by
    /// ([`Wait::WhileRuns`]): its exec was ended.
    Stalled {
        given: String,
    },
    Io(io::Error),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(error) => out.write_str(error),
            Self::NotRunning { given } => write!(out, "{given} is not running"),
            Self::Paused {
                given,
                provider: None,
            } => write!(
                out,
                "{given} is paused: its metrics can be read once it is unpaused"
            ),
            Self::Paused {
                given,
                provider: Some(provider),
            } => write!(
                out,
                "{given} runs in {provider}'s microVM, which is paused: its metrics can be read \
                 once {provider} is unpaused"
            ),
            Self::Stalled { given } => write!(
                out,
                "{given}: its microVM ran nothing for {} s, so its metrics were not read",
                TAKE_TIMEOUT.as_secs()
            ),
            Self::Io(error) => write!(out, "{:?}, native code {:?}", error.kind(), error.raw_os_error()),
        }
    }
}

/// The existing ERR stream, not part of the OTLP JSON on stdout.
struct Diagnostics<'a, 'b>(&'a Reply<'b>);

impl io::Write for Diagnostics<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.bytes(crate::spec::LOG_STDERR, bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<D: Disk> Daemon<D> {
    pub(super) fn read_metrics(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(out)) = (args.first(), asker.files.first()) else {
            let _ = Diagnostics(reply).write_all(b"shards: read metrics: missing target/output\n");
            return 1;
        };
        match self.collect_metrics(given, out, reply) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(error) => {
                let _ = writeln!(Diagnostics(reply), "shards: read metrics: {error}");
                1
            }
        }
    }

    fn collect_metrics(&self, given: &str, out: &OwnedFd, reply: &Reply<'_>) -> Result<bool, Failure> {
        let id = self.resolve(given).map_err(Failure::Resolve)?;
        self.await_start(&id);
        let not_running = || Failure::NotRunning {
            given: given.to_owned(),
        };
        if !self.running(&id) {
            return Err(not_running());
        }
        let allocation = |_| io::Error::from(io::ErrorKind::OutOfMemory);
        let mut argv = Vec::new();
        argv.try_reserve_exact(2).map_err(allocation)?;
        let (joined, vm) = {
            let runs = lock(&self.runs);
            let Some(RunState::Tracked(run)) = runs.get(&id) else {
                return Err(not_running());
            };
            // join_vm already resolves this to the actual provider at admission;
            // use its held relationship, not a second pathname walk or cache.
            let microvm = run.joined.as_deref().unwrap_or(&id);
            for id in [id.as_str(), microvm] {
                let mut name = Vec::new();
                name.try_reserve_exact(id.len()).map_err(allocation)?;
                name.extend_from_slice(id.as_bytes());
                argv.push(name);
            }
            (run.joined.clone(), run.vm.id())
        };
        // Its microVM runs none of its guest while paused, which a joiner's own pause does
        // not show: its provider's is looked at too.
        let paused_provider = {
            let paused = lock(&self.paused);
            if paused.contains(&id) {
                return Err(Failure::Paused {
                    given: given.to_owned(),
                    provider: None,
                });
            }
            joined.filter(|provider| paused.contains(provider))
        };
        if let Some(provider) = paused_provider {
            let shown = lock(&self.containers)
                .get(&provider)
                .map(|c| c.name.trim_start_matches('/').to_owned());
            return Err(Failure::Paused {
                given: given.to_owned(),
                provider: Some(shown.unwrap_or(provider)),
            });
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::METRICS,
            argv,
            ..Default::default()
        };
        let null = std::fs::File::open(crate::os::NULL)?;
        let (mut errors, into) = std::io::pipe()?;
        let ended = std::thread::scope(|scope| {
            // Its errors passed on as they come, beside the wait for its end: an exec held
            // up writing more of them than a pipe holds would never end. Once the asker can
            // be told no more, the rest are read and dropped.
            let drain = std::thread::Builder::new()
                .name("metrics-errors".into())
                .spawn_scoped(scope, move || {
                    if io::copy(&mut errors, &mut Diagnostics(reply)).is_err() {
                        let _ = io::copy(&mut errors, &mut io::sink());
                    }
                })?;
            // Waited for as long as its microVM's process runs (D209): one that runs nothing
            // (its guest paused since it was looked at, or stalled) is told SIGKILL, and its
            // end waited for, so that its output never outlives its asker.
            let ended = self.exec_on(
                &id,
                &spec,
                false,
                [null.as_fd(), out.as_fd(), into.as_fd()],
                Wait::WhileRuns(vm),
            );
            drop(into);
            let _ = drain.join();
            ended
        })?;
        if ended.timed_out {
            return Err(Failure::Stalled {
                given: given.to_owned(),
            });
        }
        Ok(ended.status == Some(0))
    }
}
