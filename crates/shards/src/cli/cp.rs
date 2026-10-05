//! `shards cp`, as docker/cli copies (cli/command/container/cp.go): between a microVM and
//! this machine, either way, as tar archives laid out by go-archive's copy rules
//! (shards_archive::copy), the daemon doing dockerd's half in the microVM (daemon
//! commands.rs, `copy_step`). The archives pass between the microVM and this process
//! through a pipe each, through no copy in the daemon.

use std::io::{IsTerminal as _, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use shards_archive::copy::{self, CopyInfo};
use shards_cmdline::flags::Parsed;

/// What the daemon is asked, and with which descriptors; its status, stdout and stderr.
type Ask<'a> =
    dyn Fn(Vec<String>, &[std::os::fd::BorrowedFd<'_>]) -> Result<(u8, Vec<u8>, Vec<u8>), String> + 'a;

/// Copies as `parsed` says, asking the daemon through `ask`.
pub fn copy(parsed: &Parsed, ask: &Ask<'_>) -> ExitCode {
    let (Some(source), Some(destination)) = (parsed.args.first(), parsed.args.get(1)) else {
        return ExitCode::FAILURE;
    };
    if source.is_empty() {
        return fail("source can not be empty");
    }
    if destination.is_empty() {
        return fail("destination can not be empty");
    }
    // Progress unless told otherwise, where stdout is a terminal (cp.go).
    let quiet = if parsed.changed("quiet") {
        parsed.bool("quiet")
    } else {
        !std::io::stdout().is_terminal()
    };
    let options = Options {
        follow: parsed.bool("follow-link"),
        uid_gid: parsed.bool("archive"),
        quiet,
    };
    let (from, src) = split(source);
    let (to, dst) = split(destination);
    let done = match (from, to) {
        (Some(c), None) => from_container(ask, &options, c, src, dst),
        (None, Some(c)) => to_container(ask, &options, c, src, dst),
        (Some(_), Some(_)) => Err("copying between containers is not supported".into()),
        (None, None) => Err("must specify at least one container source".into()),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// What writes the archive into the microVM's pipe.
type Packer<'a> = Box<dyn FnOnce() -> Result<(), String> + Send + 'a>;

struct Options {
    follow: bool,
    uid_gid: bool,
    quiet: bool,
}

fn fail(said: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "{said}");
    ExitCode::FAILURE
}

/// splitCpArg: `CONTAINER:PATH`, or a local path (absolute, or with no container before a
/// `:`, or one beginning with `.`).
fn split(arg: &str) -> (Option<&str>, &str) {
    if Path::new(arg).is_absolute() || arg.starts_with('/') {
        return (None, arg);
    }
    match arg.split_once(':') {
        Some((c, p)) if !c.starts_with('.') => (Some(c), p),
        _ => (None, arg),
    }
}

/// resolveLocalPath: absolute, its trailing `/` or `/.` kept.
fn local(path: &str) -> Result<PathBuf, String> {
    let abs = std::path::absolute(path).map_err(|e| e.to_string())?;
    Ok(copy::preserve_trailing_dot_or_separator(
        &clean(&abs),
        Path::new(path),
    ))
}

/// filepath.Clean of an absolute path.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// A path's stat in container `container`, as dockerd's PathStat has it.
struct Stat {
    size: u64,
    mode: u32,
    link_target: String,
}

impl Stat {
    fn is_dir(&self) -> bool {
        self.mode & (1 << 31) != 0
    }
    fn is_symlink(&self) -> bool {
        self.mode & (1 << 27) != 0
    }
}

/// [`Stat`] of `path` in `container`, or what dockerd said.
fn stat(ask: &Ask<'_>, container: &str, path: &str) -> Result<Stat, String> {
    let (status, out, err) = ask(step("stat", container, path), &[])?;
    if status != 0 {
        return Err(String::from_utf8_lossy(&err).trim_end().to_string());
    }
    let v: serde_json::Value = serde_json::from_slice(&out).map_err(|e| e.to_string())?;
    Ok(Stat {
        size: v.get("size").and_then(serde_json::Value::as_u64).unwrap_or(0),
        mode: v
            .get("mode")
            .and_then(serde_json::Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .unwrap_or(0),
        link_target: v
            .get("linkTarget")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn step(what: &str, container: &str, path: &str) -> Vec<String> {
    vec![
        crate::daemon::COPY_STEP.to_string(),
        what.to_string(),
        container.to_string(),
        path.to_string(),
    ]
}

/// ValidateOutputPath: a destination whose directory is there, and that is a directory, a
/// regular file, or nothing yet.
fn validate_output(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt as _;
    if let Some(dir) = clean(path).parent().filter(|d| !d.as_os_str().is_empty())
        && !dir.exists()
    {
        return Err(format!(
            "invalid output path: directory {:?} does not exist",
            dir.display().to_string()
        ));
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() || m.is_file() => Ok(()),
        Ok(m) if m.file_type().is_block_device() || m.file_type().is_char_device() => Err(format!(
            "invalid output path: {:?} must be a directory or a regular file: got a device",
            path.display().to_string()
        )),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("stat {}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// copyFromContainer: the archive of `src` in `container`, to `dst` here, or to stdout as
/// it is (`-`).
fn from_container(ask: &Ask<'_>, o: &Options, container: &str, src: &str, dst: &str) -> Result<(), String> {
    let dst = if dst == "-" {
        PathBuf::from("-")
    } else {
        local(dst)?
    };
    validate_output(&dst)?;
    let mut src = src.to_string();
    let mut rebase = Vec::new();
    // -L: a link's target, copied under the link's name.
    if o.follow
        && let Ok(s) = stat(ask, container, &src)
        && s.is_symlink()
    {
        let mut target = PathBuf::from(&s.link_target);
        if !target.is_absolute() {
            let (parent, _) = copy::split_path_dir_entry(Path::new(&src));
            target = parent.join(target);
        }
        let (target, name) = copy::get_rebase_name(Path::new(&src), &target);
        src = target.display().to_string();
        rebase = name;
    }
    let s = stat(ask, container, &src)?;
    let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let copied = AtomicU64::new(0);
    let info = CopyInfo {
        path: PathBuf::from(&src),
        exists: true,
        is_dir: s.is_dir(),
        rebase_name: rebase,
    };
    let laid = with_progress(o.quiet, "Copying from container - ", &copied, || {
        std::thread::scope(|scope| {
            let unpacking = scope.spawn(|| -> Result<(), String> {
                let content = Counted {
                    inner: reader,
                    total: &copied,
                };
                if dst == Path::new("-") {
                    let mut content = content;
                    std::io::copy(&mut content, &mut std::io::stdout().lock())
                        .map(drop)
                        .map_err(|e| e.to_string())
                } else if info.rebase_name.is_empty() {
                    copy::copy_to(content, &info, &dst).map_err(|e| e.to_string())
                } else {
                    let (_, base) = copy::split_path_dir_entry(&info.path);
                    let (r, w) = std::io::pipe().map_err(|e| e.to_string())?;
                    let (old, new) = (
                        base.as_os_str().as_encoded_bytes().to_vec(),
                        info.rebase_name.clone(),
                    );
                    std::thread::scope(|inner| {
                        let rebasing = inner
                            .spawn(move || copy::rebase_archive_entries(content, w, &old, &new).map(drop));
                        let to = copy::copy_to(r, &info, &dst).map_err(|e| e.to_string());
                        let rebased = rebasing.join().map_err(|_| "rebasing failed".to_string())?;
                        rebased.map_err(|e| e.to_string())?;
                        to
                    })
                }
            });
            let asked = ask(
                step("archive", container, &src),
                &[std::os::fd::AsFd::as_fd(&writer)],
            );
            drop(writer);
            let laid = unpacking.join().map_err(|_| "unpacking failed".to_string())?;
            match asked {
                Ok((0, _, _)) => laid,
                Ok((_, _, err)) => Err(String::from_utf8_lossy(&err).trim_end().to_string()),
                Err(e) => Err(e),
            }
        })
    });
    if !o.quiet && dst != Path::new("-") {
        let moved = copied.load(Ordering::Relaxed);
        let reported = if s.is_dir() { moved } else { s.size };
        let _ = std::io::stderr().write_all(summary(reported, moved, &dst.display().to_string()).as_bytes());
    }
    laid
}

/// copyToContainer: `src` here, or the tar archive on stdin (`-`), unpacked at `dst` in
/// `container`.
fn to_container(ask: &Ask<'_>, o: &Options, container: &str, src: &str, dst: &str) -> Result<(), String> {
    let src = if src == "-" { None } else { Some(local(src)?) };
    let mut dst_info = CopyInfo {
        path: PathBuf::from(dst),
        ..CopyInfo::default()
    };
    // What is there: a link followed once, as the CLI follows it.
    if let Ok(mut s) = stat(ask, container, dst) {
        let mut found = Ok(());
        if s.is_symlink() {
            let mut target = PathBuf::from(&s.link_target);
            if !target.is_absolute() {
                let (parent, _) = copy::split_path_dir_entry(Path::new(dst));
                target = parent.join(target);
            }
            dst_info.path = target.clone();
            match stat(ask, container, &target.display().to_string()) {
                Ok(t) => s = t,
                Err(e) => found = Err(e),
            }
        }
        if found.is_ok() {
            if s.mode & (1 << 26) != 0 {
                return Err(format!(
                    "destination \"{container}:{dst}\" must be a directory or a regular file: got a device"
                ));
            }
            dst_info.exists = true;
            dst_info.is_dir = s.is_dir();
        }
    }
    let copied = AtomicU64::new(0);
    let (reader, mut writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let (dir, content_size, packer): (PathBuf, Option<u64>, Packer<'_>) = match &src {
        None => {
            if !dst_info.is_dir {
                return Err(format!("destination \"{container}:{dst}\" must be a directory"));
            }
            let copied = &copied;
            (
                dst_info.path.clone(),
                None,
                Box::new(move || {
                    let mut stdin = Counted {
                        inner: std::io::stdin().lock(),
                        total: copied,
                    };
                    std::io::copy(&mut stdin, &mut writer)
                        .map(drop)
                        .map_err(|e| e.to_string())
                }),
            )
        }
        Some(src) => {
            let info = copy::copy_info_source_path(src, o.follow).map_err(|e| e.to_string())?;
            let size = local_size(&info.path);
            let prepared = copy::prepare_archive_copy(&info, &dst_info).map_err(|e| e.to_string())?;
            let copied = &copied;
            (
                prepared.dst_dir.clone(),
                size,
                Box::new(move || {
                    let mut out = CountedWrite {
                        inner: &mut writer,
                        total: copied,
                    };
                    match &prepared.rebase {
                        None => copy::tar_resource(&info, &mut out)
                            .map(drop)
                            .map_err(|e| e.to_string()),
                        Some((old, new)) => {
                            let (r, w) = std::io::pipe().map_err(|e| e.to_string())?;
                            std::thread::scope(|inner| {
                                let packing = inner.spawn(move || copy::tar_resource(&info, w).map(drop));
                                let rebased = copy::rebase_archive_entries(r, &mut out, old, new).map(drop);
                                let packed = packing.join().map_err(|_| "packing failed".to_string())?;
                                packed.map_err(|e| e.to_string())?;
                                rebased.map_err(|e| e.to_string())
                            })
                        }
                    }
                }),
            )
        }
    };
    let done = with_progress(o.quiet, "Copying to container - ", &copied, || {
        std::thread::scope(|scope| {
            let packing = scope.spawn(packer);
            let asked = ask(
                vec![
                    crate::daemon::COPY_STEP.to_string(),
                    "extract".into(),
                    container.to_string(),
                    dir.display().to_string(),
                    if o.uid_gid { "1" } else { "0" }.into(),
                    "0".into(),
                ],
                &[std::os::fd::AsFd::as_fd(&reader)],
            );
            drop(reader);
            let packed = packing.join().map_err(|_| "packing failed".to_string())?;
            match asked {
                Ok((0, _, _)) => packed,
                Ok((_, _, err)) => Err(String::from_utf8_lossy(&err).trim_end().to_string()),
                Err(e) => Err(e),
            }
        })
    });
    if !o.quiet {
        let moved = copied.load(Ordering::Relaxed);
        let reported = content_size.unwrap_or(moved);
        let to = format!("{container}:{}", dst_info.path.display());
        let _ = std::io::stderr().write_all(summary(reported, moved, &to).as_bytes());
    }
    done
}

/// localContentSize: a regular file's size, or the regular files' under a directory;
/// none where it cannot be read.
fn local_size(path: &Path) -> Option<u64> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_dir() {
        return Some(if m.is_file() { m.len() } else { 0 });
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let t = e.file_type().ok()?;
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                total = total.saturating_add(e.metadata().ok()?.len());
            }
        }
    }
    Some(total)
}

/// copySummary.
fn summary(content: u64, transferred: u64, dest: &str) -> String {
    if content != transferred {
        return format!(
            "Successfully copied {} (transferred {}) to {dest}\n",
            size3(content),
            size3(transferred)
        );
    }
    format!("Successfully copied {} to {dest}\n", size3(content))
}

/// go-units HumanSizeWithPrecision(n, 3): decimal units, three significant digits (%.3g).
fn size3(n: u64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = n as f64;
    let mut i = 0;
    while size >= 1000.0 && i + 1 < UNITS.len() {
        size /= 1000.0;
        i += 1;
    }
    let digits = if size >= 100.0 {
        0
    } else if size >= 10.0 {
        1
    } else {
        2
    };
    let shown = format!("{size:.digits$}");
    let shown = if shown.contains('.') {
        shown.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        shown
    };
    format!("{shown}{}", UNITS.get(i).unwrap_or(&""))
}

/// What passes counted, for the progress line and the summary.
struct Counted<'a, R> {
    inner: R,
    total: &'a AtomicU64,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

struct CountedWrite<'a, W> {
    inner: W,
    total: &'a AtomicU64,
}

impl<W: Write> Write for CountedWrite<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.total.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// copyProgress: `work`, and while it runs, on a terminal stderr, `HEADER SIZE` redrawn
/// as `total` grows, every 75 ms, the cursor hidden; the line gone again at the end.
fn with_progress<T>(quiet: bool, header: &str, total: &AtomicU64, work: impl FnOnce() -> T) -> T {
    if quiet || !std::io::stderr().is_terminal() {
        return work();
    }
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        // aec.Save, then the line.
        let _ = std::io::stderr().write_all(b"\x1b7Preparing to copy...");
        let drawing = std::thread::Builder::new()
            .name("cp-progress".into())
            .spawn_scoped(scope, move || {
                let mut err = std::io::stderr();
                let _ = err.write_all(format!("\x1b[?25l\x1b8\x1b[2K{header}{}", size3(0)).as_bytes());
                let mut last = 0;
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(std::time::Duration::from_millis(75))
                {
                    let n = total.load(Ordering::Relaxed);
                    if n != last {
                        let _ = err
                            .write_all(format!("\x1b[{}G\x1b[0K{}", header.len() + 1, size3(n)).as_bytes());
                        last = n;
                    }
                }
                // aec.Restore, aec.EraseLine(All), the cursor shown.
                let _ = err.write_all(b"\x1b8\x1b[2K\x1b[?25h");
            });
        let done = work();
        let _ = stop.send(());
        if let Ok(d) = drawing {
            let _ = d.join();
        }
        done
    })
}
