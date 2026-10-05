//! The client's terminal under `run -t`, kept as the Docker CLI keeps it
//! (docs/research/tty-and-interactive-runs.md §2.1): stdin in raw mode with moby/term's
//! flags, until the client exits by any path but SIGKILL, and the size of its stdout.

use std::io;
use std::sync::{Mutex, PoisonError};

/// The terminal's settings before raw mode, to restore.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Puts stdin's terminal in raw mode as moby/term's makeRaw does (termios_unix.go): the
/// same flags on every OS, which Apple's cfmakeraw does not set (it also sets IGNBRK and
/// clears IMAXBEL, TOSTOP and more), applied at once, as TCSETS and TIOCSETA apply them.
pub fn make_raw() -> io::Result<()> {
    // SAFETY: an all-zero termios is a valid out-parameter.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr(3) fills `t` for stdin.
    if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let saved = t;
    t.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL
        | libc::IXON);
    t.c_oflag &= !libc::OPOST;
    t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    t.c_cflag &= !(libc::CSIZE | libc::PARENB);
    t.c_cflag |= libc::CS8;
    if let Some(min) = t.c_cc.get_mut(libc::VMIN) {
        *min = 1;
    }
    if let Some(time) = t.c_cc.get_mut(libc::VTIME) {
        *time = 0;
    }
    // SAFETY: tcsetattr(3) reads `t`.
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    *SAVED.lock().unwrap_or_else(PoisonError::into_inner) = Some(saved);
    Ok(())
}

/// Gives the terminal back the settings raw mode took, if it took any. Again, it does
/// nothing.
pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap_or_else(PoisonError::into_inner).take() {
        // SAFETY: tcsetattr(3) reads `saved`.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
    }
}

/// The size of the terminal `fd` is, rows then columns, or 0×0 if it is not one, as the CLI
/// measures its stdout (docker/cli cli/streams/stream.go, GetTtySize).
pub fn size(fd: libc::c_int) -> (u16, u16) {
    // SAFETY: an all-zero winsize is a valid out-parameter.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills `ws`.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return (0, 0);
    }
    (ws.ws_row, ws.ws_col)
}

/// The colour of the terminal's background, as it answers xterm's OSC 11 query
/// (`ESC ] 11 ; ? ST`, answered `ESC ] 11 ; rgb:RRRR/GGGG/BBBB`), which xterm, iTerm2,
/// kitty, WezTerm, Ghostty, Alacritty, foot, VTE's terminals, Windows Terminal and
/// Terminal.app answer. A terminal that does not answer it is told apart without waiting
/// on a timer: the query is followed by DA1 (`ESC [ c`), which every terminal answers, and
/// an answer to DA1 with none to OSC 11 before it says there is none. Asked of the
/// controlling terminal, not stdin, which may be a pipe; `None` where there is none, or
/// it says nothing in `PATIENCE`, the most a terminal that answers neither is waited for.
pub fn background() -> Option<(u8, u8, u8)> {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    const PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();
    // SAFETY: an all-zero termios is a valid out-parameter.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr(3) fills `t` for the terminal.
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return None;
    }
    let saved = t;
    t.c_lflag &= !(libc::ECHO | libc::ICANON);
    // Reads return what there is, never wait: select(2) does the waiting.
    if let Some(min) = t.c_cc.get_mut(libc::VMIN) {
        *min = 0;
    }
    if let Some(time) = t.c_cc.get_mut(libc::VTIME) {
        *time = 0;
    }
    // SAFETY: tcsetattr(3) reads `t`.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
        return None;
    }
    let mut heard = Vec::new();
    let asked = tty.write_all(b"\x1b]11;?\x1b\\\x1b[c").and_then(|()| tty.flush());
    let deadline = std::time::Instant::now() + PATIENCE;
    if asked.is_ok() {
        let mut buf = [0u8; 256];
        // select(2), not poll(2): macOS's poll does not wait on a terminal device.
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            // SAFETY: an all-zero fd_set is empty; FD_SET marks the terminal's descriptor,
            // which is below FD_SETSIZE, as open(2) gave the lowest free one.
            let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
            if usize::try_from(fd).is_ok_and(|f| f >= libc::FD_SETSIZE) {
                break;
            }
            // SAFETY: as above.
            unsafe { libc::FD_SET(fd, &mut set) };
            let mut wait = libc::timeval {
                tv_sec: field(i64::try_from(left.as_secs()).unwrap_or(i64::MAX)),
                tv_usec: field(i64::from(left.subsec_micros())),
            };
            // SAFETY: select(2) on one descriptor, its set and timeout on this stack.
            let ready = unsafe {
                libc::select(
                    fd + 1,
                    &mut set,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut wait,
                )
            };
            if ready <= 0 {
                break;
            }
            match tty.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => heard.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(_) => break,
            }
            // DA1's answer, `ESC [ ? … c`, ends what there is to hear.
            if let Some(at) = find(&heard, b"\x1b[?")
                && heard.get(at..).is_some_and(|rest| rest.contains(&b'c'))
            {
                break;
            }
        }
    }
    // SAFETY: tcsetattr(3) reads `saved`, the settings found.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    parse_background(&heard)
}

/// `v` as a `timeval` field, whose types differ from target to target; 0 if it does not
/// fit.
fn field<T: TryFrom<i64> + Default>(v: i64) -> T {
    T::try_from(v).unwrap_or_default()
}

/// Where `needle` starts in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The colour in an answer to OSC 11: each channel of 1 to 4 hex digits, scaled to 8 bits
/// (XParseColor's `rgb:` form).
fn parse_background(heard: &[u8]) -> Option<(u8, u8, u8)> {
    let at = find(heard, b"]11;rgb:")? + 8;
    let rest = heard.get(at..)?;
    let end = rest
        .iter()
        .position(|b| *b == 0x07 || *b == 0x1b)
        .unwrap_or(rest.len());
    let text = std::str::from_utf8(rest.get(..end)?).ok()?;
    let mut channels = text.split('/').map(|h| {
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len().clamp(1, 4))) - 1;
        u8::try_from(v * 255 / max).ok()
    });
    Some((channels.next()??, channels.next()??, channels.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_is_read_from_its_answer() {
        assert_eq!(
            parse_background(b"\x1b]11;rgb:2828/2c2c/3434\x1b\\\x1b[?62;22c"),
            Some((0x28, 0x2c, 0x34))
        );
        assert_eq!(parse_background(b"\x1b]11;rgb:f/0/8\x07"), Some((255, 0, 136)));
        // DA1 alone: the terminal does not say.
        assert_eq!(parse_background(b"\x1b[?1;2c"), None);
    }
}
