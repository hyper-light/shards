//! The guest kernel and shards-init that `shards run` boots, kept by content in
//! `$SHARDS_HOME/guest`, named by their SHA-256. A run then knows what it boots by digest
//! without reading either file, and templates are kept by those digests
//! (docs/design/architecture.md D25).
//!
//! The default guest is shards' pinned kernel (kernel.rs), downloaded on first need, and
//! the shards-init this build carries (D28). `shards guest use --kernel FILE --init FILE`
//! records a guest of the user's own instead.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock, PoisonError};

use sha2::{Digest as _, Sha256};
use shards_registry::http::Cancel;

use crate::kernel::{KERNEL, Pinned};

const USAGE: &str = "usage: shards guest use --kernel FILE --init FILE
       shards guest
  use: keep FILE's kernel and shards-init as the guest `shards run` boots, by content.
  With no command: show the guest in use.
  SHARDS_KERNEL_URL: where to download the default kernel, whose SHA-256 is still checked.";

/// The shards-init this build carries, built for this host's guests by build.rs.
#[cfg(unix)]
const INIT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shards-init"));

/// A guest: its files in the store, and their digests.
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
                let home = shards_ipc::home()?;
                let (kernel, init, whose) = match current(&home)? {
                    Some(g) => (g.kernel_digest, g.init_digest, ""),
                    None => {
                        let pinned = KERNEL.ok_or(NO_KERNEL)?;
                        (
                            format!("sha256:{}", pinned.sha256),
                            init_digest()?.to_string(),
                            " (default)",
                        )
                    }
                };
                let _ = writeln!(io::stdout(), "kernel {kernel}{whose}\ninit   {init}{whose}");
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
                let g = record(&shards_ipc::home()?, &kernel, &init)?;
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

const NO_KERNEL: &str =
    "shards pins no guest kernel for this architecture: shards guest use --kernel FILE --init FILE";

/// The default guest in `home`'s store: the pinned kernel, downloaded if it is not there
/// yet, with what the download does said through `say`; and this build's init.
pub fn default(
    home: &Path,
    say: &dyn Fn(&str),
    cancel: Option<&Cancel>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Guest, String> {
    let guest = pinned(home)?;
    let dir = home.join("guest");
    let pinned = KERNEL.ok_or(NO_KERNEL)?;
    if guest.kernel.is_file() && guest.init.is_file() {
        return Ok(guest);
    }
    // One daemon serves a home, and it stores one file at a time: a run that waited finds
    // the kernel stored, and a temporary file here is one a process that ended left.
    static STORING: Mutex<()> = Mutex::new(());
    let _one = STORING.lock().unwrap_or_else(PoisonError::into_inner);
    shards_vmm::platform::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("kernel.") || name.starts_with("storing.") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if !guest.init.is_file() {
        store(&dir, &guest.init, init_bytes()?)?;
    }
    if !guest.kernel.is_file() {
        download(&dir, &guest.kernel, &pinned, say, cancel, env)?;
    }
    Ok(guest)
}

#[cfg(unix)]
fn init_bytes() -> Result<&'static [u8], String> {
    Ok(INIT)
}

#[cfg(not(unix))]
fn init_bytes() -> Result<&'static [u8], String> {
    Err("shards runs no guests on this platform yet".into())
}

/// The digest of this build's init, hashed once.
fn init_digest() -> Result<&'static str, String> {
    static DIGEST: OnceLock<String> = OnceLock::new();
    let bytes = init_bytes()?;
    Ok(DIGEST.get_or_init(|| format!("sha256:{}", hex(&Sha256::digest(bytes)))))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `bytes` to `path` in `dir` whole: into a temporary file, synced, then renamed.
fn store(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = dir.join(format!("storing.{}", std::process::id()));
    let written = File::create(&temp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&temp, path));
    written.map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("{}: {e}", path.display())
    })
}

/// Downloads `pinned` to `path` in `dir`, from `SHARDS_KERNEL_URL` if set. The body is
/// hashed as it arrives and kept only if it is the pinned kernel: its size and SHA-256.
fn download(
    dir: &Path,
    path: &Path,
    pinned: &Pinned,
    say: &dyn Fn(&str),
    cancel: Option<&Cancel>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    use shards_registry::http::{Client, Redirects, Request};
    use shards_registry::{tls, url::Url};

    let from = std::env::var("SHARDS_KERNEL_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| pinned.url.to_string());
    let failed = |e: &dyn std::fmt::Display| {
        format!(
            "downloading the guest kernel from {from}: {e}\n\
             `shards guest use --kernel FILE --init FILE` boots a kernel of your own"
        )
    };
    say(&format!(
        "Downloading the guest kernel {} from {from}",
        pinned.name
    ));
    let url = Url::parse(&from).map_err(|e| failed(&e))?;
    let config = tls::client_config(Vec::new(), None).map_err(|e| failed(&e))?;
    let http = Client::new(
        Box::new(move |_| Ok(config.clone())),
        &format!("shards/{}", env!("CARGO_PKG_VERSION")),
    );
    let http = match cancel {
        Some(cancel) => http.cancelled_by(cancel.clone()),
        None => http,
    }
    .with_proxies(shards_registry::proxy::Proxies::from_env(env));
    let request = Request {
        method: "GET",
        url: &url,
        headers: &[],
        body: &[],
        file: None,
    };
    let mut response = http
        .follow(&request, &|_| Ok(None), Redirects::Anywhere)
        .map_err(|e| failed(&e))?;
    if response.status != 200 {
        return Err(failed(&format!("HTTP {}", response.status)));
    }
    let temp = dir.join(format!("kernel.{}", std::process::id()));
    let got = (|| -> io::Result<(u64, String)> {
        let mut to = File::create(&temp)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut size = 0u64;
        loop {
            let n = match response.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let chunk = buf.get(..n).unwrap_or_default();
            size = size.saturating_add(n as u64);
            // More than the pinned kernel is not it: stop reading.
            if size > pinned.size {
                break;
            }
            hasher.update(chunk);
            to.write_all(chunk)?;
        }
        to.sync_all()?;
        Ok((size, hex(&hasher.finalize())))
    })();
    let kept = match got {
        Ok((size, digest)) if size == pinned.size && digest == pinned.sha256 => {
            fs::rename(&temp, path).map_err(|e| failed(&e))
        }
        Ok((size, _)) if size > pinned.size => Err(failed(&format!(
            "more than the pinned kernel's {} bytes",
            pinned.size
        ))),
        Ok((size, digest)) => Err(failed(&format!(
            "got {size} bytes with SHA-256 {digest}, not the pinned kernel's {} bytes with {}",
            pinned.size, pinned.sha256
        ))),
        Err(e) => Err(failed(&e)),
    };
    if kept.is_err() {
        let _ = fs::remove_file(&temp);
    }
    kept?;
    say(&format!("Guest kernel: sha256:{}", pinned.sha256));
    Ok(())
}

/// The guest recorded under `home`, if any.
/// The pinned kernel and the shards-init this build carries, where [`default`] stores
/// them in `home`, stored or not.
fn pinned(home: &Path) -> Result<Guest, String> {
    let pinned = KERNEL.ok_or(NO_KERNEL)?;
    let dir = home.join("guest");
    let init_digest = init_digest()?;
    Ok(Guest {
        kernel: dir.join(format!("sha256-{}", pinned.sha256)),
        init: dir.join(init_digest.replacen(':', "-", 1)),
        kernel_digest: format!("sha256:{}", pinned.sha256),
        init_digest: init_digest.to_string(),
    })
}

/// The guest runs of `home` boot: the one recorded, else the pinned one; `None` where
/// there is neither.
#[cfg(unix)]
pub fn in_use(home: &Path) -> Result<Option<Guest>, String> {
    match current(home)? {
        Some(recorded) => Ok(Some(recorded)),
        None => Ok(pinned(home).ok()),
    }
}

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
    // What the record names outlasts a power loss before the record does, and the record
    // is durable once recorded (audit A15).
    let sync = || shards_vmm::platform::sync_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()));
    sync()?;
    let record = format!("{kernel_digest}\n{init_digest}\n");
    let temp = dir.join(format!("current.{}", std::process::id()));
    let written = File::create(&temp)
        .and_then(|mut f| f.write_all(record.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&temp, dir.join("current")));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(format!("recording the guest: {e}"));
    }
    sync()?;
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
        let hex = hex(&hasher.finalize());
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
