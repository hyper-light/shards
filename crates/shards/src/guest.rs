//! The guest kernel and shards-init that `shards run` boots, kept by content:
//! `shards guest use --kernel FILE --init FILE` copies both into `$SHARDS_HOME/guest`, named
//! by their SHA-256, and records them as this host's guest. A run then knows what it boots
//! by digest without reading either file, and templates are kept by those digests
//! (docs/design/architecture.md D25).

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest as _, Sha256};

const USAGE: &str = "usage: shards guest use --kernel FILE --init FILE
       shards guest
  use: keep FILE's kernel and shards-init as the guest `shards run` boots, by content.
  With no command: show the guest in use.";

/// A recorded guest: its files in the store, and their digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guest {
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub kernel_digest: String,
    pub init_digest: String,
}

pub fn guest(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut args = args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
    });
    let result = (|| -> Result<(), String> {
        match args.next().transpose()?.as_deref() {
            None => {
                let home = crate::pull::home()?;
                match current(&home)? {
                    Some(g) => {
                        let _ = writeln!(
                            io::stdout(),
                            "kernel {}\ninit   {}",
                            g.kernel_digest,
                            g.init_digest
                        );
                    }
                    None => {
                        let _ = writeln!(
                            io::stdout(),
                            "no guest: shards guest use --kernel FILE --init FILE"
                        );
                    }
                }
                Ok(())
            }
            Some("use") => {
                let (mut kernel, mut init) = (None, None);
                while let Some(arg) = args.next().transpose()? {
                    let mut value = |name: &str| {
                        args.next()
                            .transpose()?
                            .ok_or_else(|| format!("{name} needs a value"))
                    };
                    match arg.as_str() {
                        "--kernel" => kernel = Some(PathBuf::from(value("--kernel")?)),
                        "--init" => init = Some(PathBuf::from(value("--init")?)),
                        other => return Err(format!("unexpected argument {other:?}")),
                    }
                }
                let (kernel, init) = (
                    kernel.ok_or("--kernel is required")?,
                    init.ok_or("--init is required")?,
                );
                let g = record(&crate::pull::home()?, &kernel, &init)?;
                let _ = writeln!(
                    io::stdout(),
                    "kernel {}\ninit   {}",
                    g.kernel_digest,
                    g.init_digest
                );
                Ok(())
            }
            Some("-h" | "--help") => {
                let _ = writeln!(io::stdout(), "{USAGE}");
                Ok(())
            }
            Some(other) => Err(format!("unknown guest command {other:?}")),
        }
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// The guest recorded under `home`, if any.
pub fn current(home: &Path) -> Result<Option<Guest>, String> {
    let record = home.join("guest").join("current");
    let text = match fs::read_to_string(&record) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", record.display())),
    };
    let mut lines = text.lines();
    let (Some(kernel), Some(init)) = (lines.next(), lines.next()) else {
        return Err(format!("{}: not a guest record", record.display()));
    };
    let dir = home.join("guest");
    let file = |digest: &str| -> Result<PathBuf, String> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| format!("{}: bad digest {digest:?}", record.display()))?;
        Ok(dir.join(format!("sha256-{hex}")))
    };
    Ok(Some(Guest {
        kernel: file(kernel)?,
        init: file(init)?,
        kernel_digest: kernel.to_string(),
        init_digest: init.to_string(),
    }))
}

/// Keeps `kernel` and `init` under `home` by content and records them as the guest.
pub fn record(home: &Path, kernel: &Path, init: &Path) -> Result<Guest, String> {
    let dir = home.join("guest");
    shards_vmm::platform::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let kernel_digest = keep(&dir, kernel)?;
    let init_digest = keep(&dir, init)?;
    let record = format!("{kernel_digest}\n{init_digest}\n");
    let temp = dir.join(format!("current.{}", std::process::id()));
    let written = File::create(&temp)
        .and_then(|mut f| f.write_all(record.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&temp, dir.join("current")));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(format!("recording the guest: {e}"));
    }
    current(home)?.ok_or_else(|| "the guest record vanished".to_string())
}

/// Copies `source` into `dir` as `sha256-<hex>`, hashing it as it goes, and returns its
/// digest. A file already stored under that name holds the same bytes, and is kept.
fn keep(dir: &Path, source: &Path) -> Result<String, String> {
    let named = |e: io::Error| format!("{}: {e}", source.display());
    let mut from = File::open(source).map_err(named)?;
    let temp = dir.join(format!("incoming.{}", std::process::id()));
    let result = (|| -> io::Result<String> {
        let mut to = File::create(&temp)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let chunk = buf.get(..n).unwrap_or_default();
            hasher.update(chunk);
            to.write_all(chunk)?;
        }
        to.sync_all()?;
        drop(to);
        let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let target = dir.join(format!("sha256-{hex}"));
        if target.is_file() {
            fs::remove_file(&temp)?;
        } else {
            fs::rename(&temp, &target)?;
        }
        Ok(format!("sha256:{hex}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(named)
}
