//! What a VM whose console no one reads keeps of it, to quote as it fails, learned for each
//! kernel (D12): its spawner reads the records its kernel's earlier failures left in
//! `<home>/said/<pin>`, gives the VM the size they need (`--console-said N`,
//! `shards_abi::bounds::console_said`), and appends how its report came out, told on a pipe
//! of its own (`--said-to`). The VM process writes no record: on macOS it opens only what its
//! spawner grants it (D30), and what it tells is held to the size it was given.
//!
//! A kernel's pin is its identity: the SHA-256 of its bytes, which the guest store names its
//! files by (guest.rs), shards' pinned kernels' among them; a kernel given by path is hashed
//! once for each file as it is, kept in `<home>/said/kernels`. A restore's kernel is not known
//! to its spawner: its records are [`UNPINNED`]'s.
//!
//! Each record is one whole line, appended with one write (`O_APPEND` on Unix,
//! `FILE_APPEND_DATA` alone on Windows), so writers beside each other (the daemon, CLIs,
//! shards-api's clients) never interleave theirs; readers pass over a line not finished.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use shards_abi::bounds::{ConsoleRecords, ConsoleReport, console_said};

/// The directory of a home's records.
pub const DIR: &str = "said";

/// The records of restores, whose kernel their spawner does not know.
pub const UNPINNED: &str = "unpinned";

/// The digests of kernels given by path, by each file's identity ([`pin`]).
const KERNELS: &str = "kernels";

/// The longest line a VM tells its spawner, [`ConsoleReport::line`]'s: `report`, a space, a
/// `usize`'s most digits, and its newline.
const LINE: usize = "report ".len() + (usize::MAX.ilog10() as usize + 1) + "\n".len();

/// What a spawner learned for one VM: its kernel's pin, and the bytes it keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Learned {
    pub pin: String,
    pub kept: usize,
}

/// What `home`'s records say a VM of `kernel` keeps ([`console_said`]); a restore's
/// (`kernel` none) [`UNPINNED`]'s. Never fails a boot: a kernel whose pin cannot be read
/// learns from every kernel's records, as one with none of its own does.
pub fn learn(home: &Path, kernel: Option<&Path>) -> Learned {
    let pin = match kernel.map(|k| pin(home, k)) {
        None => UNPINNED.to_string(),
        Some(Ok(pin)) => pin,
        Some(Err(e)) => {
            shards_vmm::debug!("the console's records: {e}");
            String::new()
        }
    };
    let dir = home.join(DIR);
    let own = if pin.is_empty() {
        ConsoleRecords::default()
    } else {
        read(&dir.join(&pin))
    };
    let kept = console_said(own, every(&dir));
    Learned { pin, kept }
}

/// The records in `file`, none where it cannot be read.
fn read(file: &Path) -> ConsoleRecords {
    match std::fs::read(file) {
        Ok(bytes) => ConsoleRecords::read(&bytes),
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                shards_vmm::debug!("{}: {e}", file.display());
            }
            ConsoleRecords::default()
        }
    }
}

/// Every kernel's records in `dir`.
fn every(dir: &Path) -> ConsoleRecords {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ConsoleRecords::default();
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n == UNPINNED || is_digest(n))
        })
        .fold(ConsoleRecords::default(), |all, e| all.and(read(&e.path())))
}

/// Whether `name` is a SHA-256 in hex, as a pin is.
fn is_digest(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `kernel`'s pin: the SHA-256 of its bytes. One in `home`'s guest store is named by it
/// (`sha256-<hex>`, which the store wrote it under as it hashed it); any other is hashed,
/// once for each file as it is ([`KERNELS`]).
pub fn pin(home: &Path, kernel: &Path) -> Result<String, String> {
    let named = |e: io::Error| format!("{}: {e}", kernel.display());
    let kernel = std::path::absolute(kernel).map_err(named)?;
    let store = std::path::absolute(home.join("guest")).map_err(named)?;
    if kernel.parent() == Some(store.as_path())
        && let Some(hex) = kernel
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("sha256-"))
            .filter(|h| is_digest(h))
    {
        return Ok(hex.to_string());
    }
    let meta = std::fs::metadata(&kernel).map_err(named)?;
    let cache = home.join(DIR).join(KERNELS).join(identity(&kernel, &meta));
    if let Ok(kept) = std::fs::read_to_string(&cache)
        && is_digest(kept.trim_end())
    {
        return Ok(kept.trim_end().to_string());
    }
    let digest = digest(&kernel).map_err(named)?;
    if let Err(e) = keep_digest(&cache, &digest) {
        shards_vmm::debug!("{}: {e}", cache.display());
    }
    Ok(digest)
}

/// The name a file's digest is kept under: the SHA-256 of what says it is the same file with
/// the same bytes, its path, length and time of change, and on Unix its device and inode.
fn identity(path: &Path, meta: &std::fs::Metadata) -> String {
    let mut h = Sha256::new();
    h.update(path.as_os_str().as_encoded_bytes());
    h.update([0]);
    h.update(meta.len().to_le_bytes());
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    h.update(modified.to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        h.update(meta.dev().to_le_bytes());
        h.update(meta.ino().to_le_bytes());
        h.update(meta.ctime().to_le_bytes());
        h.update(meta.ctime_nsec().to_le_bytes());
    }
    hex(&h.finalize())
}

/// The SHA-256 of `path`'s bytes, in hex.
fn digest(path: &Path) -> io::Result<String> {
    use std::io::BufRead as _;
    // Read through std's own buffer, its size std's choice.
    let mut file = io::BufReader::new(std::fs::File::open(path)?);
    let mut h = Sha256::new();
    loop {
        let n = match file.fill_buf() {
            Ok([]) => break,
            Ok(buf) => {
                h.update(buf);
                buf.len()
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        file.consume(n);
    }
    Ok(hex(&h.finalize()))
}

/// Keeps `digest` at `cache`, whole or not at all: written beside it, then renamed.
fn keep_digest(cache: &Path, digest: &str) -> io::Result<()> {
    let dir = cache.parent().ok_or_else(|| io::Error::other("no directory"))?;
    crate::os::owner_only_dir(std::fs::DirBuilder::new().recursive(true)).create(dir)?;
    let temp = dir.join(format!(".{}.{}", std::process::id(), random()?));
    let written = std::fs::write(&temp, digest).and_then(|()| std::fs::rename(&temp, cache));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// 64 random bits, in hex: a temporary file's name no other writer of the same pid takes.
fn random() -> io::Result<String> {
    let mut b = [0u8; 8];
    shards_vmm::platform::fill_random(&mut b)?;
    Ok(hex(&b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Appends to `home`'s records for `pin` how a VM given `kept` bytes said its report came
/// out, `told` on its pipe: nothing where it told nothing (it did not fail, or ended before
/// it could). What it tells is held to what it was given: a whole report no longer than
/// `kept`, and a cut at `kept` itself.
pub fn record(home: &Path, pin: &str, kept: usize, told: &[u8]) -> Result<(), String> {
    if told.is_empty() || pin.is_empty() {
        return Ok(());
    }
    let report = told
        .strip_suffix(b"\n")
        .and_then(ConsoleReport::parse)
        .ok_or_else(|| format!("the VM told {:?}, no report", String::from_utf8_lossy(told)))?;
    let report = match report {
        ConsoleReport::Whole(n) if n <= kept => report,
        ConsoleReport::Whole(n) => {
            return Err(format!(
                "the VM told a whole report of {n} bytes, past the {kept} it kept"
            ));
        }
        ConsoleReport::Cut(_) => ConsoleReport::Cut(kept),
    };
    if pin != UNPINNED && !is_digest(pin) {
        return Err(format!("{pin:?} is no kernel's pin"));
    }
    let dir = home.join(DIR);
    crate::os::owner_only_dir(std::fs::DirBuilder::new().recursive(true))
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join(pin);
    append(&file, report.line().as_bytes()).map_err(|e| format!("{}: {e}", file.display()))
}

/// Appends `line` to `file` in one write, which no other writer's append interleaves: its
/// end found and written at once (`O_APPEND`, POSIX write(2); `FILE_APPEND_DATA` without
/// `FILE_WRITE_DATA`, std's `append`, on Windows). One cut short (a full disk) ends in no
/// newline, and readers pass over it.
fn append(file: &Path, line: &[u8]) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(file)?;
    let n = f.write(line)?;
    if n != line.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("{n} of the record's {} bytes written", line.len()),
        ));
    }
    Ok(())
}

/// What a VM told on its pipe to its end, its one line at most: more is no report.
pub fn told(from: impl Read) -> io::Result<Vec<u8>> {
    let mut told = Vec::new();
    // One byte past the longest line, to see one longer.
    from.take(u64::try_from(LINE).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut told)?;
    Ok(told)
}

/// `shards console-said HOME PIN KEPT`, started by a directly booted VM's command line before
/// it becomes the VM (`shards run vm --kernel`, `shards restore vm` on Unix): reads what the
/// VM tells on descriptor 3 to its end, as the VM exits, and records it. Not for people to run.
#[cfg(unix)]
pub fn recorder(mut args: impl Iterator<Item = std::ffi::OsString>) -> std::process::ExitCode {
    use std::os::fd::FromRawFd as _;
    let recorded = (|| -> Result<(), String> {
        let home = PathBuf::from(args.next().ok_or("no home")?);
        let pin = args.next().and_then(|a| a.into_string().ok()).ok_or("no pin")?;
        let kept: usize = args
            .next()
            .and_then(|a| a.into_string().ok())
            .and_then(|a| a.parse().ok())
            .ok_or("no size kept")?;
        // SAFETY: fcntl(2) asks whether descriptor 3, which its starter gives it, is open.
        if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
            return Err("no pipe at descriptor 3".into());
        }
        // SAFETY: the pipe its starter gave it at 3 (checked open above), owned from here on.
        let from = unsafe { std::fs::File::from_raw_fd(3) };
        let told = told(from).map_err(|e| format!("the VM's pipe: {e}"))?;
        record(&home, &pin, kept, &told)
    })();
    match recorded {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards: the console's record: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The `--console-said` given in a VM's arguments `args` (before any `--`), and whether
/// they give `--said-to`, where its starter takes what it tells itself.
pub fn given(args: &[std::ffi::OsString]) -> (Option<usize>, bool) {
    let mut kept = None;
    let mut to = false;
    let mut words = args.iter().take_while(|a| *a != "--");
    while let Some(w) = words.next() {
        if w == "--console-said" {
            kept = words.next().and_then(|n| n.to_str()).and_then(|n| n.parse().ok());
        } else if w == "--said-to" {
            to = true;
        }
    }
    (kept, to)
}

/// Whether the VM `args` start (`run …` or `restore …`) quotes its console as it fails: one
/// whose console no one reads (`--no-console`), that runs no command and is no daemon's
/// warm VM, whose starter takes no report of it already (`--said-to`). Its kernel, a
/// run's (`--kernel`), or none for a restore.
pub fn quotes(args: &[std::ffi::OsString]) -> Option<Option<PathBuf>> {
    let first = args.first().and_then(|a| a.to_str())?;
    if !matches!(first, "run" | "restore") {
        return None;
    }
    let mut kernel = None;
    let mut discard = false;
    let mut words = args.iter().skip(1).cloned();
    while let Some((w, value)) = crate::spec::next_word(&mut words) {
        match w.to_str() {
            Some("--" | "--warm" | "--resume" | "--said-to") => return None,
            Some("--no-console") => discard = true,
            Some("--kernel") if first == "run" => kernel = value.map(PathBuf::from),
            _ => {}
        }
    }
    if !discard {
        return None;
    }
    match first {
        "run" => kernel.map(Some),
        _ => Some(None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn words(w: &[&str]) -> Vec<OsString> {
        w.iter().map(OsString::from).collect()
    }

    /// A kernel's records teach what its VMs keep: none, a panic's frame; its own whole
    /// reports and cuts after; another kernel's only for one with none of its own. What a VM
    /// tells is held to what it was given, and a cut recorded at it.
    #[test]
    fn a_kernels_records_teach_what_its_vms_keep() {
        let dir = shards_testdir::TempDir::new("said-learn").unwrap();
        let home = dir.path();
        let a = "a".repeat(64);
        let frame = shards_abi::bounds::PANIC_FRAME;
        let kernel_a = home.join("guest").join(format!("sha256-{a}"));
        std::fs::create_dir_all(kernel_a.parent().unwrap()).unwrap();
        std::fs::write(&kernel_a, b"not hashed: named by its digest").unwrap();
        let learned = learn(home, Some(&kernel_a));
        assert_eq!(
            learned,
            Learned {
                pin: a.clone(),
                kept: frame
            }
        );
        record(home, &a, frame, b"report 1129\n").unwrap();
        assert_eq!(learn(home, Some(&kernel_a)).kept, 1129);
        // Nothing told, nothing recorded; a lie refused.
        record(home, &a, 1129, b"").unwrap();
        assert!(record(home, &a, 1129, b"report 99999\n").is_err());
        assert!(record(home, &a, 1129, b"report 10").is_err());
        assert!(record(home, "../x", 1129, b"report 10\n").is_err());
        // A cut is recorded at what was kept, whatever the VM says.
        record(home, &a, 1129, b"cut 7\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(DIR).join(&a)).unwrap(),
            "report 1129\ncut 1129\n"
        );
        assert_eq!(learn(home, Some(&kernel_a)).kept, 2258);
        // Another kernel learns from every kernel's until it has its own.
        let kernel_b = home.join("vmlinux");
        std::fs::write(&kernel_b, b"kernel b").unwrap();
        let learned_b = learn(home, Some(&kernel_b));
        let hashed = hex(&Sha256::digest(b"kernel b"));
        assert_eq!(
            learned_b,
            Learned {
                pin: hashed.clone(),
                kept: 2258
            }
        );
        record(home, &hashed, 2258, b"report 300\n").unwrap();
        assert_eq!(learn(home, Some(&kernel_b)).kept, 300);
        // A restore's, unpinned, and an unreadable kernel's, learn from every kernel's.
        assert_eq!(
            learn(home, None),
            Learned {
                pin: UNPINNED.into(),
                kept: 2258
            }
        );
        assert_eq!(learn(home, Some(&home.join("absent"))).kept, 2258);
    }

    /// A kernel given by path is hashed once for each file as it is: its digest is kept, and
    /// a file changed is hashed again.
    #[test]
    fn a_path_kernels_digest_is_kept_until_it_changes() {
        let dir = shards_testdir::TempDir::new("said-pin").unwrap();
        let home = dir.path();
        let kernel = home.join("Image");
        std::fs::write(&kernel, b"one").unwrap();
        assert_eq!(pin(home, &kernel).unwrap(), hex(&Sha256::digest(b"one")));
        let kept: Vec<_> = std::fs::read_dir(home.join(DIR).join(KERNELS)).unwrap().collect();
        assert_eq!(kept.len(), 1);
        // The kept digest is what is read back: a stand-in kept is believed.
        let cache = kept[0].as_ref().unwrap().path();
        std::fs::write(&cache, "c".repeat(64)).unwrap();
        assert_eq!(pin(home, &kernel).unwrap(), "c".repeat(64));
        std::fs::write(&kernel, b"three").unwrap();
        assert_eq!(pin(home, &kernel).unwrap(), hex(&Sha256::digest(b"three")));
        // One in the store is named by its digest; one named so elsewhere is hashed.
        let elsewhere = home.join(format!("sha256-{}", "d".repeat(64)));
        std::fs::write(&elsewhere, b"four").unwrap();
        assert_eq!(pin(home, &elsewhere).unwrap(), hex(&Sha256::digest(b"four")));
    }

    /// Writers beside each other append whole lines, every one read back.
    #[test]
    fn records_appended_at_once_never_interleave() {
        let dir = shards_testdir::TempDir::new("said-append").unwrap();
        let home = dir.path();
        let pin = "e".repeat(64);
        std::thread::scope(|s| {
            for t in 0..8usize {
                let pin = &pin;
                s.spawn(move || {
                    for i in 0..200usize {
                        record(
                            home,
                            pin,
                            1_000_000,
                            format!("report {}\n", t * 1000 + i).as_bytes(),
                        )
                        .unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(home.join(DIR).join(&pin)).unwrap();
        let mut lines: Vec<usize> = text
            .lines()
            .map(|l| l.strip_prefix("report ").unwrap().parse().unwrap())
            .collect();
        lines.sort_unstable();
        let mut want: Vec<usize> = (0..8).flat_map(|t| (0..200).map(move |i| t * 1000 + i)).collect();
        want.sort_unstable();
        assert_eq!(lines, want);
    }

    /// Only a VM that quotes its console learns: one whose console no one reads, running
    /// no command, no daemon's, whose starter takes no report itself.
    #[test]
    fn only_a_vm_that_quotes_its_console_learns() {
        let run = |w: &[&str]| quotes(&words(w));
        assert_eq!(
            run(&["run", "--kernel", "k", "--no-console"]),
            Some(Some(PathBuf::from("k")))
        );
        assert_eq!(run(&["run", "--kernel", "k"]), None);
        assert_eq!(run(&["run", "--kernel", "k", "--no-console", "--", "true"]), None);
        assert_eq!(
            run(&["run", "--kernel", "k", "--no-console", "--warm", "3"]),
            None
        );
        assert_eq!(
            run(&["run", "--kernel", "k", "--no-console", "--said-to", "3"]),
            None
        );
        // A flag's value is its own, whatever it reads as.
        assert_eq!(run(&["run", "--kernel", "k", "--cmdline", "--no-console"]), None);
        assert_eq!(run(&["restore", "dir", "--no-console"]), Some(None));
        assert_eq!(run(&["restore", "dir", "--no-console", "--resume", "3"]), None);
        assert_eq!(run(&["pull", "x"]), None);
        assert_eq!(
            given(&words(&["run", "--console-said", "300", "--said-to", "4"])),
            (Some(300), true)
        );
        assert_eq!(
            given(&words(&["run", "--", "--console-said", "300"])),
            (None, false)
        );
    }

    /// What a VM tells is read to its end, its one line at most.
    #[test]
    fn a_vms_word_is_one_line_at_most() {
        assert_eq!(told(&b"report 18446744073709551615\n"[..]).unwrap().len(), LINE);
        assert_eq!(told(&[b'x'; 100][..]).unwrap().len(), LINE + 1);
    }
}
