//! `inotify DIR`: DIR watched with inotify(7) for every event, each said as it comes,
//! `inotify MASK NAME`, MASK its bits' names joined by `|` and NAME empty for DIR's own;
//! `inotify ready` once the watch is set. What a guest's watcher sees of its host's edits
//! (D235).

use std::io::Write as _;

/// Each of inotify's bits a watch of every event is told, by name (inotify(7)).
const BITS: [(u32, &str); 16] = [
    (libc::IN_ACCESS, "IN_ACCESS"),
    (libc::IN_MODIFY, "IN_MODIFY"),
    (libc::IN_ATTRIB, "IN_ATTRIB"),
    (libc::IN_CLOSE_WRITE, "IN_CLOSE_WRITE"),
    (libc::IN_CLOSE_NOWRITE, "IN_CLOSE_NOWRITE"),
    (libc::IN_OPEN, "IN_OPEN"),
    (libc::IN_MOVED_FROM, "IN_MOVED_FROM"),
    (libc::IN_MOVED_TO, "IN_MOVED_TO"),
    (libc::IN_CREATE, "IN_CREATE"),
    (libc::IN_DELETE, "IN_DELETE"),
    (libc::IN_DELETE_SELF, "IN_DELETE_SELF"),
    (libc::IN_MOVE_SELF, "IN_MOVE_SELF"),
    (libc::IN_UNMOUNT, "IN_UNMOUNT"),
    (libc::IN_Q_OVERFLOW, "IN_Q_OVERFLOW"),
    (libc::IN_IGNORED, "IN_IGNORED"),
    (libc::IN_ISDIR, "IN_ISDIR"),
];

pub fn main(dir: &str) -> i32 {
    let Ok(path) = std::ffi::CString::new(dir) else {
        let _ = writeln!(std::io::stderr(), "inotify: a path with NUL");
        return 2;
    };
    // SAFETY: inotify_init1(2) with a flag.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
    if fd < 0 || unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_ALL_EVENTS) } < 0 {
        let _ = writeln!(std::io::stderr(), "inotify: {dir}: {}", std::io::Error::last_os_error());
        return 1;
    }
    let mut out = std::io::stdout();
    let _ = writeln!(out, "inotify ready");
    let _ = out.flush();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: read(2) into a buffer of its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        let Ok(n) = usize::try_from(n) else {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return 1;
        };
        // Each event: wd, mask, cookie, len (u32 each), then a name of len bytes,
        // NUL-padded.
        let mut at = 0;
        while let Some(head) = buf.get(at..at + 16).filter(|_| at + 16 <= n) {
            let word = |i: usize| {
                u32::from_ne_bytes(
                    head.get(i..i + 4)
                        .and_then(|w| w.try_into().ok())
                        .unwrap_or_default(),
                )
            };
            let (mask, len) = (word(4), word(12) as usize);
            let name = buf.get(at + 16..at + 16 + len).unwrap_or_default();
            let name = String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or_default());
            let bits: Vec<&str> = BITS.iter().filter(|(b, _)| mask & b != 0).map(|(_, s)| *s).collect();
            let _ = writeln!(out, "inotify {} {name}", bits.join("|"));
            at += 16 + len;
        }
        let _ = out.flush();
    }
}
