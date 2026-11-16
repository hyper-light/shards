//! What shards takes on disk, as its user sets it (`shards configure disk`, D186): how much
//! the build cache keeps, and how much of a run's output its log keeps. A line a setting,
//! `NAME VALUE`, in `$SHARDS_HOME/disk`, which each build's collection and the daemon, as
//! each run starts, read as they need them: a change takes at the next build or run,
//! whether the daemon runs or not. A setting not there is its default: the build cache's
//! shares of the disk its store is on (it reserves 10%, keeps at most 80%, and keeps 20%
//! free), and at most 1 GiB of a run's newest output in its log (the user's calls,
//! 2026-10-18), and of the home's newest events in its journal, the home's own log (D187).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// An amount of a disk: a share of it, or bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amount {
    Percent(u64),
    Bytes(u64),
}

impl Amount {
    /// `N%` of the disk, 0 to 100, or a size as `--memory` takes one (`50g`, `512m`).
    fn parse(text: &str) -> Result<Amount, String> {
        match text.strip_suffix('%') {
            Some(p) => match p.trim().parse::<u64>() {
                Ok(p) if p <= 100 => Ok(Amount::Percent(p)),
                _ => Err(format!("{text}: a share of the disk is 0% to 100%")),
            },
            None => {
                let b = shards_cmdline::resources::ram_in_bytes(text)?;
                u64::try_from(b)
                    .map(Amount::Bytes)
                    .map_err(|_| format!("{text}: not a size"))
            }
        }
    }

    fn shown(self) -> String {
        match self {
            Amount::Percent(p) => format!("{p}%"),
            Amount::Bytes(b) => b.to_string(),
        }
    }
}

/// The settings, each `None` where none is set: its default holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings {
    pub cache_reserve: Option<Amount>,
    pub cache_max: Option<Amount>,
    pub cache_free: Option<Amount>,
    /// Bytes of a run's newest output its log keeps.
    pub log_history: Option<u64>,
    /// Bytes of the home's newest events its journal keeps (D187).
    pub events_history: Option<u64>,
}

pub const CACHE_RESERVE: Amount = Amount::Percent(10);
pub const CACHE_MAX: Amount = Amount::Percent(80);
pub const CACHE_FREE: Amount = Amount::Percent(20);
pub const LOG_HISTORY: u64 = 1 << 30;

/// How a log of `history` bytes keeps its run's newest output (runs-11 `LOG_SEGMENT`): in
/// `files` equal segments of `size` bytes, the oldest going as a new one begins, so that it
/// holds at most its history, and at least its history less two segments (one gone at a
/// rotation, and what a whole number of them leaves of it).
///
/// A segment's size trades two costs. Each segment costs its making and its rotation (a
/// warm VM's `LOG_SEGMENT` ask, two files made and two removed, its pack) and a name each
/// reader lists (`LogFile::segments`): `history / size` of them. A reader resuming from a
/// cursor (`Pos`, its segment's number and its record's) finds its place within that one
/// segment, by its index and, packed, its seek table's frame (packed.rs), and the cap loses
/// a segment of what it keeps at each rotation: `size` bytes. Counted in the file system's
/// own unit `unit`, an allocation unit read or written per segment against one per unit
/// of a segment, their sum `history / size + size / unit` is least at
/// `size = √(history · unit)`; whole units, since each file takes whole units of the disk.
/// Two segments at least, as one segment would leave nothing at its rotation. A history
/// smaller than two units, and a home whose unit could not be read (`None`, a cold start),
/// keep two halves: the bound the cap alone sets. 1 GiB on 4 KiB units: 512 segments of
/// 2 MiB.
// The daemon's (Unix's alone as yet).
#[cfg_attr(not(unix), allow(dead_code))]
pub const fn log_retention(history: u64, unit: Option<u64>) -> crate::spec::LogRetention {
    if history < 2 {
        return crate::spec::LogRetention {
            size: history,
            files: 1,
        };
    }
    let half = history / 2;
    let size = match unit {
        Some(unit) if unit > 0 && unit <= half => {
            let balanced = (history as u128 * unit as u128).isqrt();
            // √ of a u128 fits a u64; the bounds keep it within [unit, half].
            let balanced = if balanced > half as u128 {
                half
            } else {
                balanced as u64
            };
            let whole = balanced / unit * unit;
            if whole < unit { unit } else { whole }
        }
        _ => half,
    };
    crate::spec::LogRetention {
        size,
        files: history / size,
    }
}

const NAMES: [&str; 5] = [
    "build-cache-reserve",
    "build-cache-max",
    "build-cache-free",
    "log-history",
    "events-history",
];

const USAGE: &str = "usage: shards configure disk [--build-cache-reserve N%|SIZE] [--build-cache-max N%|SIZE]
                             [--build-cache-free N%|SIZE] [--log-history SIZE]
                             [--events-history SIZE] [--reset]
  How much of the disk shards keeps, each setting until set again; alone, what each is.
  --build-cache-reserve: what the build cache always keeps, at most its most (default 10%
    of its disk)
  --build-cache-max: the most it keeps (default 80% of its disk; 0 for no most)
  --build-cache-free: the disk it keeps free, past its reserve (default 20%; 0 for none)
  --log-history: the most a run's log keeps of its newest output (default 1g)
  --events-history: the most the home keeps of its newest events, for `--since`
    (default 1g)
  --reset: every setting back to its default";

/// The settings' file in the home, and the lock its writers hold (D186).
const NAME: &str = "disk";
const LOCK: &str = "disk.lock";

fn path(home: &Path) -> PathBuf {
    home.join(NAME)
}

/// The settings `home` keeps: none where its file is not there.
pub fn read(home: &Path) -> Result<Settings, String> {
    let file = path(home);
    let fail = |e: std::io::Error| format!("{}: {e}", file.display());
    // Its newest version: Windows' writes land versions beside it (platform `write_in`).
    let opened =
        shards_vmm::platform::open_dir(home).and_then(|dir| shards_vmm::platform::open_newest_in(&dir, NAME));
    let text = match opened.and_then(std::io::read_to_string) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(fail(e)),
    };
    let mut settings = Settings::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = || format!("{}:{}", file.display(), n + 1);
        let (name, value) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("{}: NAME VALUE", at()))?;
        settings
            .set(name, value.trim())
            .map_err(|e| format!("{}: {e}", at()))?;
    }
    Ok(settings)
}

/// Writes `settings` to `home`'s file whole, from a staging file of this writer's own,
/// synced and renamed over the last (platform `write_in`), so that a reader finds the one
/// or the other and no other writer's staging is touched.
fn write(home: &Path, settings: &Settings) -> Result<(), String> {
    let file = path(home);
    let mut text = String::from("# shards configure disk: what shards keeps on disk (D186)\n");
    for (name, value) in settings.set_ones() {
        text.push_str(&format!("{name} {value}\n"));
    }
    let fail = |e: std::io::Error| format!("{}: {e}", file.display());
    let dir = shards_vmm::platform::open_dir(home).map_err(fail)?;
    shards_vmm::platform::write_in(&dir, NAME, text.as_bytes()).map_err(fail)
}

/// Applies `given`, settings in order (`None` a reset), to what `home` keeps, as one
/// transaction: the file is reread, changed and written under the home's settings lock, so
/// that of two commands neither writes over what the other kept from a stale read. The
/// lock is a stable file of its own, as the settings' file is replaced; it is unlocked
/// before it closes (PM M244).
fn update(home: &Path, given: &[Option<(&str, &str)>]) -> Result<Settings, String> {
    if given.is_empty() {
        return read(home);
    }
    let lock = home.join(LOCK);
    let _writing =
        shards_image::store::Locked::exclusive(&lock).map_err(|e| format!("{}: {e}", lock.display()))?;
    // A reset sets aside what was there, which then need not read.
    let last_reset = given.iter().rposition(Option::is_none);
    let mut settings = match last_reset {
        Some(_) => Settings::default(),
        None => read(home)?,
    };
    for (name, value) in given.iter().skip(last_reset.map_or(0, |at| at + 1)).flatten() {
        settings.set(name, value)?;
    }
    write(home, &settings)?;
    Ok(settings)
}

impl Settings {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        match name {
            "build-cache-reserve" => self.cache_reserve = Some(Amount::parse(value)?),
            "build-cache-max" => self.cache_max = Some(Amount::parse(value)?),
            "build-cache-free" => self.cache_free = Some(Amount::parse(value)?),
            "log-history" | "events-history" => {
                let bytes = shards_cmdline::resources::ram_in_bytes(value)?;
                let bytes = u64::try_from(bytes)
                    .ok()
                    .filter(|b| *b > 0)
                    .ok_or_else(|| format!("{value}: a log keeps at least a byte"))?;
                match name {
                    "log-history" => self.log_history = Some(bytes),
                    _ => self.events_history = Some(bytes),
                }
            }
            other => {
                return Err(format!(
                    "{other}: no such setting (the settings: {})",
                    NAMES.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Each setting that is set, by name, as its file says it.
    fn set_ones(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(a) = self.cache_reserve {
            out.push(("build-cache-reserve", a.shown()));
        }
        if let Some(a) = self.cache_max {
            out.push(("build-cache-max", a.shown()));
        }
        if let Some(a) = self.cache_free {
            out.push(("build-cache-free", a.shown()));
        }
        if let Some(b) = self.log_history {
            out.push(("log-history", b.to_string()));
        }
        if let Some(b) = self.events_history {
            out.push(("events-history", b.to_string()));
        }
        out
    }

    /// How a log keeps its run's newest output ([`log_retention`]), on a file system that
    /// allocates in `unit`s.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn log_retention(&self, unit: Option<u64>) -> crate::spec::LogRetention {
        log_retention(self.log_history.unwrap_or(LOG_HISTORY), unit)
    }

    /// How the home's journal keeps its newest events: as a log keeps its run's output
    /// ([`log_retention`]), the home's own log, by the same default (D187).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn events_retention(&self, unit: Option<u64>) -> crate::spec::LogRetention {
        log_retention(self.events_history.unwrap_or(LOG_HISTORY), unit)
    }
}

/// `shards configure disk [...]` (`disk configure`): sets what its flags give, or with none
/// says each setting and whether it is its default.
pub fn command(args: impl Iterator<Item = std::ffi::OsString>) -> ExitCode {
    let args: Vec<String> = match args.map(|a| a.into_string()).collect::<Result<_, _>>() {
        Ok(args) => args,
        Err(a) => return failed(&format!("argument {a:?} is not valid UTF-8")),
    };
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let flags = [
        "--build-cache-reserve",
        "--build-cache-max",
        "--build-cache-free",
        "--log-history",
        "--events-history",
    ];
    if crate::context::asks_help(&words, &flags) {
        return crate::context::help_page("disk", USAGE);
    }
    let result = (|| -> Result<(), String> {
        let mut words = words.into_iter();
        match words.next() {
            Some("configure") => {}
            _ => return Err(USAGE.into()),
        }
        // Each setting given, in order, `None` for a reset; each checked before any is kept.
        let mut given: Vec<Option<(&str, &str)>> = Vec::new();
        while let Some(word) = words.next() {
            if word == "--reset" {
                given.push(None);
                continue;
            }
            let (flag, value) = match word.split_once('=') {
                Some((f, v)) => (f, v),
                None => (
                    word,
                    words.next().ok_or_else(|| format!("{word}: needs a value"))?,
                ),
            };
            let name = flag
                .strip_prefix("--")
                .filter(|n| NAMES.contains(n))
                .ok_or_else(|| format!("{flag}: no such setting\n{USAGE}"))?;
            Settings::default().set(name, value)?;
            given.push(Some((name, value)));
        }
        let home = shards_ipc::home()?;
        let settings = update(&home, &given)?;
        // The build cache collected by its new bounds at the next build.
        if given
            .iter()
            .any(|g| g.is_none_or(|(name, _)| name.starts_with("build-cache-")))
        {
            crate::build::gc::collect_next(&home)?;
        }
        let shown = |set: Option<String>, default: String| match set {
            Some(v) => v,
            None => format!("{default} (default)"),
        };
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "build-cache-reserve {}",
            shown(settings.cache_reserve.map(Amount::shown), CACHE_RESERVE.shown())
        );
        let _ = writeln!(
            out,
            "build-cache-max     {}",
            shown(settings.cache_max.map(Amount::shown), CACHE_MAX.shown())
        );
        let _ = writeln!(
            out,
            "build-cache-free    {}",
            shown(settings.cache_free.map(Amount::shown), CACHE_FREE.shown())
        );
        let _ = writeln!(
            out,
            "log-history         {}",
            shown(
                settings.log_history.map(|b| b.to_string()),
                LOG_HISTORY.to_string()
            )
        );
        let _ = writeln!(
            out,
            "events-history      {}",
            shown(
                settings.events_history.map(|b| b.to_string()),
                LOG_HISTORY.to_string()
            )
        );
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

fn failed(why: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {why}");
    ExitCode::FAILURE
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Two commands' updates of different settings both stay: the later reads the file
    /// only once the earlier, holding the settings lock, has written its own (runs-09).
    #[test]
    fn concurrent_settings_updates_keep_each_others_fields() {
        let home = shards_testdir::TempDir::new("disk-race").unwrap();
        let mut earlier = Settings::default();
        earlier.set("log-history", "1m").unwrap();
        write(&home, &earlier).unwrap();
        // Another command's transaction, under way: it holds the lock while it writes.
        let held = shards_image::store::Locked::exclusive(&home.join(LOCK)).unwrap();
        let (send, done) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| send.send(update(&home, &[Some(("log-history", "4m"))])).unwrap());
            // Unfixed, the update reads, changes and writes at once; the other command's
            // write from its own read then drops it.
            let early = done.recv_timeout(std::time::Duration::from_secs(1));
            earlier.set("events-history", "2m").unwrap();
            write(&home, &earlier).unwrap();
            drop(held);
            early.or_else(|_| done.recv()).unwrap().unwrap();
        });
        let kept = read(&home).unwrap();
        assert_eq!(kept.log_history, Some(4 << 20));
        assert_eq!(kept.events_history, Some(2 << 20));
    }

    /// A write stages its bytes in a file of its own: another writer's staging is neither
    /// truncated nor published by it (runs-09).
    #[test]
    fn settings_publication_leaves_another_writers_staging_file_alone() {
        let home = shards_testdir::TempDir::new("disk-stage").unwrap();
        let other = home.join(".disk.new");
        std::fs::write(&other, b"another writer's bytes").unwrap();
        let mut settings = Settings::default();
        settings.set("log-history", "4m").unwrap();
        write(&home, &settings).unwrap();
        assert_eq!(std::fs::read(&other).unwrap(), b"another writer's bytes");
        assert_eq!(read(&home).unwrap().log_history, Some(4 << 20));
    }

    /// A setting reads back as written, one not set is its default, and a log's history
    /// becomes equal segments of √(history · unit) in whole units, two at least, that hold
    /// no more than it (runs-11 `LOG_SEGMENT`). Red before: 20 MiB segments whatever the
    /// unit, 52 of 1 GiB.
    #[test]
    fn settings_read_back_and_come_to_their_amounts() {
        let home = shards_testdir::TempDir::new("disk-settings").unwrap();
        assert_eq!(read(&home).unwrap(), Settings::default());
        let mut s = Settings::default();
        s.set("build-cache-max", "50%").unwrap();
        s.set("build-cache-reserve", "60g").unwrap();
        s.set("log-history", "64k").unwrap();
        write(&home, &s).unwrap();
        let back = read(&home).unwrap();
        assert_eq!(back, s);
        let kept = back.log_retention(Some(4 << 10));
        assert_eq!((kept.size, kept.files), (16 << 10, 4));
        let default = Settings::default().log_retention(Some(4 << 10));
        assert_eq!((default.size, default.files), (2 << 20, 512));
        // 64 KiB units (an NTFS cluster's most common large size): √(2^30 · 2^16) = 8 MiB.
        let default = Settings::default().log_retention(Some(64 << 10));
        assert_eq!((default.size, default.files), (8 << 20, 128));
        for (history, unit, size, files) in [
            (0, Some(4096), 0, 1),
            (1, Some(4096), 1, 1),
            (3, Some(4096), 1, 3),
            // Less than two units: two halves.
            (6 << 10, Some(4096), 3 << 10, 2),
            // √(100 MiB · 4 KiB) = 640 KiB.
            (100 << 20, Some(4096), 640 << 10, 160),
            // Not a whole number of segments: what is left over is not kept.
            ((100 << 20) + 1, Some(4096), 640 << 10, 160),
            // A unit not read: two halves, the cap's own bound.
            (1 << 30, None, 1 << 29, 2),
        ] {
            let s = Settings {
                log_history: Some(history),
                ..Settings::default()
            };
            let kept = s.log_retention(unit);
            assert_eq!((kept.size, kept.files), (size, files), "{history} {unit:?}");
            assert!(kept.size * kept.files <= history);
            if let Some(unit) = unit.filter(|&u| history >= 2 * u) {
                assert_eq!(kept.size % unit, 0, "{history}");
                assert!(kept.files >= 2);
            }
        }
        assert!(s.set("build-cache-free", "101%").is_err());
        assert!(s.set("log-history", "0").is_err());
        // The home's events: kept as a log keeps a run's output, by its default.
        assert_eq!(
            Settings::default().events_retention(Some(4096)),
            Settings::default().log_retention(Some(4096))
        );
        s.set("events-history", "64k").unwrap();
        assert_eq!(s.events_history, Some(64 << 10));
        assert_eq!(
            s.events_retention(Some(4096)),
            log_retention(64 << 10, Some(4096))
        );
        assert!(s.set("events-history", "0").is_err());
        assert!(s.set("cache", "1g").is_err());
    }
}
