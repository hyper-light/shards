//! On-demand OTLP artifacts on the owner's existing output descriptor.

use std::io::{self, Write as _};

use shards_ipc::fd::{AsFd as _, OwnedFd};

use super::commands::{Asker, Reply};
use super::{Daemon, Disk, RunState, lock};

enum Failure {
    Resolve(String),
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
        if !self.running(&id) || lock(&self.paused).contains(&id) {
            return Err(io::Error::from(io::ErrorKind::NotConnected).into());
        }
        let allocation = |_| io::Error::from(io::ErrorKind::OutOfMemory);
        let mut argv = Vec::new();
        argv.try_reserve_exact(2).map_err(allocation)?;
        {
            let runs = lock(&self.runs);
            let Some(RunState::Tracked(run)) = runs.get(&id) else {
                return Err(io::Error::from(io::ErrorKind::NotConnected).into());
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
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::METRICS,
            argv,
            ..Default::default()
        };
        let null = std::fs::File::open(crate::os::NULL)?;
        let (mut errors, into) = std::io::pipe()?;
        let ended = self.exec_on(&id, &spec, false, [null.as_fd(), out.as_fd(), into.as_fd()]);
        drop(into);
        io::copy(&mut errors, &mut Diagnostics(reply))?;
        Ok(ended?.status == Some(0))
    }
}
