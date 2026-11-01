//! What a rename over a file meets on Windows (PM M345, D196). The snapshot store replaces a
//! snapshot's pointer and a generation's working set by a POSIX rename over the file
//! (FileRenameInformationEx, FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS),
//! as `write_in` does (crates/vmm/src/platform/windows_files.rs), and under a full test run
//! eight writers' renames over one working set were refused at once (STATUS_ACCESS_DENIED).
//! 1. A rename over a file another handle holds open sharing all (M344's case), one a section
//!    of which is made but not mapped, one a view of which is mapped, and one whose view is
//!    all that is left of it.
//! 2. Writers as `write_in`'s: a file written to a name of its own, flushed, renamed over the
//!    target, flushed and closed, then the next at once; one writer, then eight side by side,
//!    then eight beside eight threads writing and removing files of their own, as a test
//!    run's other tests write theirs. Each refused rename's status, who Restart Manager says
//!    holds the target then, how long until the same rename, retried every 100 us, is taken,
//!    and whether the held target can be moved away, or linked aside, for the file to be
//!    renamed in instead.
//! 3. A directory renamed with a file in it open, then closed; and generations committed as
//!    the store commits them, files written and closed, then their directory renamed into
//!    place: whether such a rename is refused too, alone and beside churn.
//!
//! In the directory named and in the user's temporary directory, where shards' tests write.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use whp_run::summary;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS, FileRenameInformationEx,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{CloseHandle, NTSTATUS};
use windows_sys::Win32::Storage::FileSystem::{
    CreateHardLinkW, DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_WRITE, FlushFileBuffers, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY, UnmapViewOfFile};
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};

/// The bytes each writer writes: a working set of a few hundred pages' size.
const BYTES: usize = 8 << 10;
/// Rounds of the one writer, and of each of the eight.
const ROUNDS: usize = 2000;
const WRITERS: usize = 8;
const EACH: usize = 250;
/// How long a refused rename is retried before it counts as never taken.
const GIVE_UP: Duration = Duration::from_secs(60);

/// What a writer does with a refused rename: retries it until taken, timed; retries it,
/// having asked Restart Manager who holds the target; moves the target away and renames its
/// file in; or links the target under a second name and renames its file over the first,
/// which then leaves the target's file a link and is no delete of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Time,
    Ask,
    Away,
    Link,
}
/// write_in's rename: over what is there, whoever holds it open.
const REPLACE: u32 = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
/// Refusals whose holders are asked of Restart Manager.
const ASKED: usize = 20;
/// The churn beside the writers: threads, and the bytes of each file they write.
const CHURNERS: usize = 8;
const CHURN: usize = 256 << 10;
/// Generations committed in a row, and the bytes of each one's memory file.
const GENERATIONS: usize = 500;
const MEMORY_BYTES: usize = 1 << 20;

/// Renames the file `file` holds to `name` in the directory `dir` holds, with `flags`.
fn rename(file: &File, dir: &File, name: &str, flags: u32) -> NTSTATUS {
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let size = size_of::<FILE_RENAME_INFORMATION>() + wide.len() * 2;
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buf` holds `size` bytes, aligned for the structure, with room for the name.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = dir.as_raw_handle();
        (*info).FileNameLength = (wide.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), wide.len());
        let mut status = IO_STATUS_BLOCK::default();
        NtSetInformationFile(file.as_raw_handle(), &mut status, info.cast(), size as u32, FileRenameInformationEx)
    }
}

fn flush(file: &File) -> Result<(), String> {
    // SAFETY: an open handle of this probe's.
    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
        return Err(format!("flush: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// `n` bytes no two files share, as a working set's are: xorshift64 from `seed`.
fn varied(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n.div_ceil(8))
        .flat_map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()
        })
        .take(n)
        .collect()
}

/// A new file `name` in `dir`, written and flushed, open to be renamed.
fn written(dir: &Path, name: &str, seed: u64) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_WRITE | DELETE)
        .open(dir.join(name))
        .map_err(|e| format!("{name}: {e}"))?;
    file.write_all(&varied(seed, BYTES)).map_err(|e| format!("{name}: {e}"))?;
    flush(&file)?;
    Ok(file)
}

/// Files written and removed in `dir` until `stop`, by `threads` threads, as the other tests
/// of a run write theirs: the disk and its scanners kept busy beside the writers.
fn churn(dir: &Path, threads: usize, stop: &std::sync::atomic::AtomicBool) -> Result<usize, String> {
    use std::sync::atomic::Ordering;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::thread::scope(|scope| {
        let each: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    let mut made = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        let path = dir.join(format!("churn-{t}-{made}.bin"));
                        let bytes = varied((t as u64) << 32 | made as u64, CHURN);
                        if std::fs::write(&path, bytes).is_ok() {
                            made += 1;
                        }
                        let _ = std::fs::remove_file(&path);
                    }
                    made
                })
            })
            .collect();
        each.into_iter()
            .map(|h| h.join().map_err(|_| "a churn thread panicked".to_string()))
            .sum()
    })
}

/// The processes Restart Manager says hold `path` open.
fn holders(path: &Path) -> String {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: a session key buffer of the size the API asks for; ended below.
    let started = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
    if started != 0 {
        return format!("RmStartSession {started}");
    }
    let names = [wide.as_ptr()];
    let mut infos = vec![RM_PROCESS_INFO::default(); 16];
    let (mut needed, mut n, mut reasons) = (0u32, infos.len() as u32, 0u32);
    // SAFETY: one NUL-terminated path, and room for `n` entries.
    let said = unsafe {
        let registered = RmRegisterResources(session, 1, names.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null());
        if registered != 0 {
            RmEndSession(session);
            return format!("RmRegisterResources {registered}");
        }
        let listed = RmGetList(session, &mut needed, &mut n, infos.as_mut_ptr(), &mut reasons);
        RmEndSession(session);
        listed
    };
    if said != 0 {
        return format!("RmGetList {said} ({needed} needed)");
    }
    let named: Vec<String> = infos
        .iter()
        .take(n as usize)
        .map(|i| {
            let end = i.strAppName.iter().position(|c| *c == 0).unwrap_or(i.strAppName.len());
            format!("{} (pid {})", String::from_utf16_lossy(&i.strAppName[..end]), i.Process.dwProcessId)
        })
        .collect();
    if named.is_empty() { "none listed".into() } else { named.join(", ") }
}

/// A rename over a file in each of the ways something else may hold it.
fn held(dir: &Path, handle: &File) -> Result<(), String> {
    let target = dir.join("held");
    for case in ["a handle sharing all", "a section, no view", "a mapped view", "a view alone"] {
        let _ = std::fs::remove_file(&target);
        std::fs::write(&target, [7u8; BYTES]).map_err(|e| e.to_string())?;
        let holder = File::open(&target).map_err(|e| e.to_string())?;
        let mut section = std::ptr::null_mut();
        let mut view = None;
        if case != "a handle sharing all" {
            // SAFETY: a read-only section of a file this probe holds open, closed below.
            section = unsafe {
                CreateFileMappingW(holder.as_raw_handle(), std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null())
            };
            if section.is_null() {
                return Err(format!("CreateFileMappingW: {}", std::io::Error::last_os_error()));
            }
        }
        if case == "a mapped view" || case == "a view alone" {
            // SAFETY: a view of the section above, unmapped below.
            let v = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, 0) };
            if v.Value.is_null() {
                return Err(format!("MapViewOfFile: {}", std::io::Error::last_os_error()));
            }
            view = Some(v);
        }
        let holder = if case == "a view alone" {
            // SAFETY: the section's handle, which the view outlives.
            unsafe { CloseHandle(section) };
            section = std::ptr::null_mut();
            drop(holder);
            None
        } else {
            Some(holder)
        };
        let file = written(dir, "held.tmp", 9)?;
        let first = varied(9, 1);
        let status = rename(&file, handle, "held", REPLACE);
        let after = std::fs::read(&target).map_err(|e| e.to_string())?;
        println!(
            "a rename over a file held by {case}: {:#010x}; the name then reads the {} bytes",
            status as u32,
            if after.first() == first.first() { "new" } else { "old" }
        );
        drop(file);
        let _ = std::fs::remove_file(dir.join("held.tmp"));
        if let Some(v) = view {
            // SAFETY: the view mapped above.
            unsafe { UnmapViewOfFile(v) };
        }
        if !section.is_null() {
            // SAFETY: the section made above.
            unsafe { CloseHandle(section) };
        }
        drop(holder);
    }
    Ok(())
}

/// Directories as the snapshot store commits generations: each `.g-N.tmp` made, a memory
/// file of `MEMORY_BYTES` and a state file written into it, flushed and closed, then the
/// directory renamed to `g-N` at once, replacing nothing; a refused rename retried every
/// 100 us until taken, as long as `GIVE_UP`. First a directory one file of which is open,
/// renamed, then with the file closed.
fn generations(place: &str, dir: &Path, churn_too: bool) -> Result<(), String> {
    let gens = dir.join("generations");
    std::fs::create_dir_all(&gens).map_err(|e| e.to_string())?;
    if !churn_too {
        let open = gens.join("open");
        std::fs::create_dir(&open).map_err(|e| e.to_string())?;
        let file = File::create(open.join("memory")).map_err(|e| e.to_string())?;
        let held = std::fs::rename(&open, gens.join("open-moved"));
        drop(file);
        let closed = std::fs::rename(&open, gens.join("open-moved"));
        println!("{place}: a directory a file of which is open, renamed: {held:?}; the file closed, renamed: {closed:?}");
    }
    let what = if churn_too { "generations beside churn" } else { "generations" };
    let stop = std::sync::atomic::AtomicBool::new(false);
    let churned = dir.with_extension("churn");
    let (mut taken, mut refused) = (Vec::new(), Vec::new());
    std::thread::scope(|scope| -> Result<(), String> {
        let (churned, stop) = (&churned, &stop);
        let load = churn_too.then(|| scope.spawn(move || churn(churned, CHURNERS, stop)));
        let made = (|| -> Result<(), String> {
            for n in 0..GENERATIONS {
                let staging = gens.join(format!(".g-{n}.tmp"));
                std::fs::create_dir(&staging).map_err(|e| e.to_string())?;
                for (name, bytes) in [("memory", MEMORY_BYTES), ("state", 4096)] {
                    let mut file = File::create(staging.join(name)).map_err(|e| e.to_string())?;
                    file.write_all(&varied(n as u64, bytes)).map_err(|e| e.to_string())?;
                    flush(&file)?;
                }
                let started = Instant::now();
                let first = std::fs::rename(&staging, gens.join(format!("g-{n}")));
                if first.is_ok() {
                    taken.push(started.elapsed());
                    continue;
                }
                let mut again = None;
                while started.elapsed() < GIVE_UP {
                    std::thread::sleep(Duration::from_micros(100));
                    if std::fs::rename(&staging, gens.join(format!("g-{n}"))).is_ok() {
                        again = Some(started.elapsed());
                        break;
                    }
                }
                println!("{place}, {what}: generation {n}: refused {first:?}, taken after {again:?}");
                refused.push(again);
            }
            Ok(())
        })();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(load) = load {
            let made = load.join().map_err(|_| "the churn panicked")??;
            println!("{place}, {what}: {made} files of {} KiB written and removed beside", CHURN >> 10);
        }
        made
    })?;
    println!("{place}, {what}: renames taken at once (us): {}", summary(&mut taken));
    let mut later: Vec<Duration> = refused.iter().filter_map(|t| *t).collect();
    println!(
        "{place}, {what}: refused {} times; taken when retried (us): {}; never within {} s: {}",
        refused.len(),
        summary(&mut later),
        GIVE_UP.as_secs(),
        refused.iter().filter(|t| t.is_none()).count()
    );
    let _ = std::fs::remove_dir_all(&gens);
    Ok(())
}

#[derive(Default)]
struct Seen {
    taken: Vec<Duration>,
    refused: Vec<(u32, Option<Duration>)>,
    asked: Vec<String>,
    /// Flushes after the rename refused, the file replaced by another writer's since.
    flushes_refused: Vec<String>,
}

/// `rounds` writes as write_in makes them, each over the last, by writer `w`. With `ask`,
/// the holders of the target as a rename is refused are asked of Restart Manager, which
/// delays the retries: those rounds are not the ones timed. Each refusal is said as it ends.
fn writer(
    dir: &Path,
    handle: &File,
    label: &str,
    w: usize,
    rounds: usize,
    mode: Mode,
    seen: &Mutex<Seen>,
) -> Result<(), String> {
    let target = dir.join("target");
    for round in 0..rounds {
        let tmp = format!("target.{w}.{round}.tmp");
        let file = written(dir, &tmp, (w as u64) << 32 | round as u64)?;
        let started = Instant::now();
        let status = rename(&file, handle, "target", REPLACE);
        let took = started.elapsed();
        if status >= 0 {
            seen.lock().map_err(|_| "poisoned")?.taken.push(took);
        } else if mode == Mode::Away {
            // The held target moved away by a handle of its own, then this file renamed in,
            // replacing nothing.
            let opened = OpenOptions::new()
                .access_mode(DELETE | SYNCHRONIZE)
                .open(&target)
                .map_err(|e| e.to_string());
            let moved = opened
                .as_ref()
                .map(|t| rename(t, handle, &format!("away.{w}.{round}"), FILE_RENAME_POSIX_SEMANTICS) as u32);
            let ours = rename(&file, handle, "target", FILE_RENAME_POSIX_SEMANTICS);
            println!(
                "{label}: writer {w} round {round}: refused {:#010x}; the target opened to move {}, moved {:x?}; this file in {:#010x}",
                status as u32,
                if opened.is_ok() { "ok".to_string() } else { format!("{opened:?}") },
                moved.ok(),
                ours as u32
            );
            seen.lock().map_err(|_| "poisoned")?.refused.push((status as u32, None));
        } else if mode == Mode::Link {
            let link = dir.join(format!("link.{w}.{round}"));
            let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
            // SAFETY: two NUL-terminated paths.
            let linked = unsafe { CreateHardLinkW(wide(&link).as_ptr(), wide(&target).as_ptr(), std::ptr::null()) };
            let linked = if linked != 0 { "ok".to_string() } else { std::io::Error::last_os_error().to_string() };
            let started = Instant::now();
            let over = rename(&file, handle, "target", REPLACE);
            let took = started.elapsed();
            println!(
                "{label}: writer {w} round {round}: refused {:#010x}; the target linked {linked}; then this file over it {:#010x} in {:.1} us",
                status as u32,
                over as u32,
                took.as_secs_f64() * 1e6
            );
            seen.lock()
                .map_err(|_| "poisoned")?
                .refused
                .push((status as u32, (over >= 0).then_some(took)));
        } else {
            let asking = mode == Mode::Ask && seen.lock().map_err(|_| "poisoned")?.asked.len() < ASKED;
            let who = asking.then(|| holders(&target));
            // Retried every 100 us: how long the target is held, and what each try says.
            let mut statuses = vec![status as u32];
            let mut again = None;
            while started.elapsed() < GIVE_UP {
                let s = rename(&file, handle, "target", REPLACE);
                if s >= 0 {
                    again = Some(started.elapsed());
                    break;
                }
                if statuses.last() != Some(&(s as u32)) {
                    statuses.push(s as u32);
                }
                std::thread::sleep(Duration::from_micros(100));
            }
            // Never taken: which is held, this file (moved to a name of its own, replacing
            // nothing) or the target (a new file renamed over it).
            let stuck = if again.is_none() {
                let own = rename(&file, handle, &format!("{tmp}.own"), FILE_RENAME_POSIX_SEMANTICS);
                let fresh = written(dir, &format!("{tmp}.fresh"), 1)?;
                let over = rename(&fresh, handle, "target", REPLACE);
                // Still held: linked under a second name, so that a rename over it deletes
                // no file.
                let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
                let link = dir.join(format!("{tmp}.link"));
                // SAFETY: two NUL-terminated paths.
                let linked = unsafe { CreateHardLinkW(wide(&link).as_ptr(), wide(&target).as_ptr(), std::ptr::null()) };
                let linked = if linked != 0 { "ok".to_string() } else { std::io::Error::last_os_error().to_string() };
                let linked_over = rename(&fresh, handle, "target", REPLACE);
                format!(
                    "; this file to a name of its own {:#010x}, a new file over the target {:#010x}; the target linked {linked}, then the new file over it {:#010x}; holders now [{}]",
                    own as u32,
                    over as u32,
                    linked_over as u32,
                    holders(&target)
                )
            } else {
                String::new()
            };
            println!(
                "{label}: writer {w} round {round}: refused {statuses:x?}, taken after {again:?}{}{stuck}",
                who.as_ref().map(|h| format!("; holders [{h}]")).unwrap_or_default()
            );
            let mut s = seen.lock().map_err(|_| "poisoned")?;
            s.refused.push((status as u32, again));
            if let Some(who) = who {
                s.asked.push(who);
            }
        }
        if let Err(e) = flush(&file) {
            seen.lock().map_err(|_| "poisoned")?.flushes_refused.push(e);
        }
    }
    Ok(())
}

fn report(what: &str, seen: Seen) {
    let mut taken = seen.taken;
    println!("{what}: renames taken at once (us): {}", summary(&mut taken));
    let mut flushes = seen.flushes_refused;
    flushes.sort();
    flushes.dedup();
    println!("{what}: flushes after the rename refused: [{}]", flushes.join("; "));
    let mut statuses: Vec<u32> = seen.refused.iter().map(|(s, _)| *s).collect();
    statuses.sort();
    statuses.dedup();
    for status in statuses {
        let of: Vec<_> = seen.refused.iter().filter(|(s, _)| *s == status).collect();
        let mut later: Vec<Duration> = of.iter().filter_map(|(_, t)| *t).collect();
        let never = of.len() - later.len();
        println!(
            "{what}: refused {status:#010x} {} times; taken when retried (us): {}; never within {} s: {never}",
            of.len(),
            summary(&mut later),
            GIVE_UP.as_secs()
        );
    }
    let mut asked = seen.asked;
    asked.sort();
    let mut counted: Vec<(String, usize)> = Vec::new();
    for a in asked {
        match counted.last_mut() {
            Some((last, n)) if *last == a => *n += 1,
            _ => counted.push((a, 1)),
        }
    }
    for (who, n) in counted {
        println!("{what}: Restart Manager's holders, {n} times: {who}");
    }
}

/// The passes in `dir` whose names hold `only`, or all: one writer, then eight, alone and
/// beside churn, retrying a refused rename, asking who holds the target, moving it away, or
/// linking it.
fn passes(place: &str, dir: &Path, only: Option<&str>) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .map_err(|e| e.to_string())?;
    if only.is_none() {
        held(dir, &handle)?;
        let seen = Mutex::new(Seen::default());
        writer(dir, &handle, &format!("{place}, one writer"), 0, ROUNDS, Mode::Time, &seen)?;
        report(&format!("{place}, one writer"), seen.into_inner().map_err(|_| "poisoned")?);
    }
    let churned = dir.with_extension("churn");
    for (pass, (what, mode, load)) in [
        ("eight writers", Mode::Time, false),
        ("eight writers, the target linked", Mode::Link, false),
        ("eight writers beside churn", Mode::Time, true),
        ("eight writers beside churn, holders asked", Mode::Ask, true),
        ("eight writers beside churn, the target moved away", Mode::Away, true),
        ("eight writers beside churn, the target linked", Mode::Link, true),
    ]
    .into_iter()
    .enumerate()
    .filter(|(_, (what, _, _))| only.is_none_or(|word| what.contains(word)))
    {
        let seen = Mutex::new(Seen::default());
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| -> Result<(), String> {
            let (churned, stop) = (&churned, &stop);
            let load = load.then(|| scope.spawn(move || churn(churned, CHURNERS, stop)));
            let each: Vec<_> = (0..WRITERS)
                .map(|w| {
                    let (handle, seen) = (&handle, &seen);
                    // Each pass's writers name their files apart from the last's.
                    let w = w + 1 + pass * WRITERS;
                    let label = format!("{place}, {what}");
                    scope.spawn(move || writer(dir, handle, &label, w, EACH, mode, seen))
                })
                .collect();
            let wrote: Result<Vec<()>, String> = each
                .into_iter()
                .map(|one| one.join().map_err(|_| "a writer panicked".to_string())?)
                .collect();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(load) = load {
                let made = load.join().map_err(|_| "the churn panicked")??;
                println!("{place}, {what}: {made} files of {} KiB written and removed beside", CHURN >> 10);
            }
            wrote.map(|_| ())
        })?;
        report(&format!("{place}, {what}"), seen.into_inner().map_err(|_| "poisoned")?);
    }
    for churn_too in [false, true] {
        let what = if churn_too { "generations beside churn" } else { "generations" };
        if only.is_none_or(|word| what.contains(word)) {
            generations(place, dir, churn_too)?;
        }
    }
    let _ = std::fs::remove_dir_all(&churned);
    Ok(())
}

fn main() {
    // The directory named on the command line, and one in the user's temporary directory,
    // where shards' tests write (shards_testdir).
    let named = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("m345-rename-{}", std::process::id())));
    let temp = std::env::temp_dir().join(format!("m345-rename-{}", std::process::id()));
    // A word of the passes to run, else all of them.
    let only = std::env::args().nth(2);
    let mut failed = false;
    for (place, dir) in [("named", named), ("temp", temp)] {
        let result = passes(place, &dir, only.as_deref());
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = result {
            println!("{place}: stopped: {e}");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}
