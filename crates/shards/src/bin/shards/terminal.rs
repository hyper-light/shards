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
