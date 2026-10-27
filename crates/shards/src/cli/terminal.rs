//! The client's terminal under `run -t`, kept as the Docker CLI keeps it
//! (docs/research/tty-and-interactive-runs.md §2.1): stdin in raw mode with moby/term's
//! flags, until the client exits by any path but SIGKILL, and the size of its stdout. On
//! Windows its console, a context's command's terminal (D180): raw as moby/term makes one
//! but for the records of its resizes, which it keeps ([`ConsoleInput`]), its size, and
//! whether it takes escape sequences, for shards' own look too.

use std::io;
use std::sync::{Mutex, PoisonError};

/// A line of stdin, its echo off while it is read, as moby/term's DisableEcho turns it
/// off for a password (prompt.DisableInputEcho), and on again however the read went; its
/// line end left off.
#[cfg(unix)]
pub(crate) fn read_without_echo() -> std::io::Result<String> {
    // SAFETY: an all-zero termios is a valid out-parameter.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr(3) fills `t` for stdin.
    if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let saved = t;
    t.c_lflag &= !libc::ECHO;
    // SAFETY: tcsetattr(3) reads `t`.
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    // SAFETY: tcsetattr(3) reads `saved`, the settings found.
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// [`read_without_echo`] on Windows: the console's input echo off while it is read.
#[cfg(windows)]
pub(crate) fn read_without_echo() -> std::io::Result<String> {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: GetStdHandle reads this process's state.
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0;
    // SAFETY: the console's own handle, its mode read, then set with one flag less.
    let echoing =
        unsafe { GetConsoleMode(h, &mut mode) != 0 && SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) != 0 };
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if echoing {
        // SAFETY: the mode read before, set again.
        unsafe { SetConsoleMode(h, mode) };
    }
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// The terminal's settings before raw mode, to restore.
#[cfg(unix)]
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Puts stdin's terminal in raw mode as moby/term's makeRaw does (termios_unix.go): the
/// same flags on every OS, which Apple's cfmakeraw does not set (it also sets IGNBRK and
/// clears IMAXBEL, TOSTOP and more), applied at once, as TCSETS and TIOCSETA apply them.
#[cfg(unix)]
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
#[cfg(unix)]
pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap_or_else(PoisonError::into_inner).take() {
        // SAFETY: tcsetattr(3) reads `saved`.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
    }
}

/// The size of the terminal `fd` is, rows then columns, or 0×0 if it is not one, as the CLI
/// measures its stdout (docker/cli cli/streams/stream.go, GetTtySize).
#[cfg(unix)]
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
#[cfg(unix)]
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
/// Whether this process's stdout (`fd` 1) or stderr (2) is a terminal that takes escape
/// sequences.
#[cfg(unix)]
pub fn styles(fd: libc::c_int) -> bool {
    // SAFETY: isatty(3) on one of this process's descriptors.
    unsafe { libc::isatty(fd) == 1 }
}

/// This process's stdin, where it is a console: its handle.
#[cfg(windows)]
fn console_in() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};
    // SAFETY: GetStdHandle and GetConsoleMode read this process's state; a handle that is
    // no console fails GetConsoleMode.
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        (!h.is_null()
            && h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
            && GetConsoleMode(h, &mut mode) != 0)
            .then_some(h)
    }
}

/// Whether this process's stdin (`fd` 0), stdout (1) or stderr (2) is a console.
#[cfg(windows)]
pub fn is_console(fd: i32) -> bool {
    match fd {
        0 => console_in().is_some(),
        _ => console(fd).is_some(),
    }
}

/// The console modes raw mode took, to give back: stdin's, and stdout's where it is one.
#[cfg(windows)]
static SAVED: Mutex<Option<(u32, Option<u32>)>> = Mutex::new(None);

/// stdin's console mode raw, from `was`, as moby/term v0.5.2 makes one (termios_windows.go,
/// makeRaw): no echo, no line editing, and ^C a key like any (no ENABLE_PROCESSED_INPUT),
/// nor mouse events; insert and quick-edit mode, which it sets with the extended flags.
/// Its window events it keeps, where moby/term turns them off and docker/cli looks at the
/// console's size every 250 ms instead (MonitorTtySize): the records of its screen
/// buffer's size are how [`ConsoleInput`] tells a resize as it comes.
#[cfg(windows)]
fn raw_mode(was: u32) -> u32 {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_INSERT_MODE, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT,
    };
    (was & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_MOUSE_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_WINDOW_INPUT
        | ENABLE_EXTENDED_FLAGS
        | ENABLE_INSERT_MODE
        | ENABLE_QUICK_EDIT_MODE
}

/// Puts stdin's console in raw mode ([`raw_mode`]), its keys the escape sequences a
/// terminal sends where the console gives them (ENABLE_VIRTUAL_TERMINAL_INPUT, as moby/term
/// asks for it where it is taken), and has stdout take escape sequences, which a terminal's
/// command writes.
#[cfg(windows)]
pub fn make_raw() -> io::Result<()> {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, SetConsoleMode,
    };
    let input = console_in().ok_or_else(|| io::Error::other("stdin is not a console"))?;
    let mut was = 0;
    // SAFETY: the console's own handle, its mode read into a local.
    if unsafe { GetConsoleMode(input, &mut was) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let raw = raw_mode(was);
    // SAFETY: the console's own handle, its mode set.
    let set = unsafe {
        SetConsoleMode(input, raw | ENABLE_VIRTUAL_TERMINAL_INPUT) != 0 || SetConsoleMode(input, raw) != 0
    };
    if !set {
        return Err(io::Error::last_os_error());
    }
    let out = console(1).and_then(|h| {
        let mut mode = 0;
        // SAFETY: the console's own handle, its mode read into a local.
        (unsafe { GetConsoleMode(h, &mut mode) } != 0).then_some(mode)
    });
    let _ = styles(1);
    *SAVED.lock().unwrap_or_else(PoisonError::into_inner) = Some((was, out));
    Ok(())
}

/// Gives the console back the modes raw mode took, if it took any. Again, it does nothing.
#[cfg(windows)]
pub fn restore() {
    use windows_sys::Win32::System::Console::SetConsoleMode;
    let Some((input, output)) = SAVED.lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    if let Some(h) = console_in() {
        // SAFETY: the console's own handle, its mode set back.
        unsafe { SetConsoleMode(h, input) };
    }
    if let (Some(h), Some(mode)) = (console(1), output) {
        // SAFETY: as above.
        unsafe { SetConsoleMode(h, mode) };
    }
}

/// This process's stdout (`fd` 1) or stderr (2), where it is a console: its handle.
#[cfg(windows)]
fn console(fd: i32) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    let which = if fd == 2 {
        STD_ERROR_HANDLE
    } else {
        STD_OUTPUT_HANDLE
    };
    // SAFETY: GetStdHandle and GetConsoleMode read this process's state; a handle that is
    // no console fails GetConsoleMode.
    unsafe {
        let h = GetStdHandle(which);
        let mut mode = 0;
        (!h.is_null()
            && h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
            && GetConsoleMode(h, &mut mode) != 0)
            .then_some(h)
    }
}

/// The terminal's background colour, asked as Unix's [`background`] asks it (OSC 11, then
/// DA1, which every terminal answers), of the console this process's stdin and stdout are:
/// stdout taking escape sequences ([`styles`]), and stdin in virtual terminal input
/// (ENABLE_VIRTUAL_TERMINAL_INPUT) for the answer, which comes as the console's input, as a
/// terminal's keys do. `None` where they are no console, or it says nothing in `PATIENCE`;
/// the console's input mode is given back as it was.
#[cfg(windows)]
pub fn background() -> Option<(u8, u8, u8)> {
    use std::io::Write as _;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW,
        SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    const PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);
    let input = console_in()?;
    if !styles(1) {
        return None;
    }
    let mut was = 0;
    // SAFETY: the console's own handle, its mode read into a local.
    if unsafe { GetConsoleMode(input, &mut was) } == 0 {
        return None;
    }
    // SAFETY: the console's own handle, its mode set; given back below.
    if unsafe { SetConsoleMode(input, raw_mode(was) | ENABLE_VIRTUAL_TERMINAL_INPUT) } == 0 {
        return None;
    }
    let mut heard = Vec::new();
    let mut out = io::stdout();
    let asked = out.write_all(b"\x1b]11;?\x1b\\\x1b[c").and_then(|()| out.flush());
    let deadline = std::time::Instant::now() + PATIENCE;
    while asked.is_ok()
        && let Some(left) = deadline.checked_duration_since(std::time::Instant::now())
    {
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the console's input handle, waited on until input comes or `ms` pass.
        if unsafe { WaitForSingleObject(input, ms) } != WAIT_OBJECT_0 {
            break;
        }
        let mut records = [INPUT_RECORD::default(); 64];
        let mut n = 0u32;
        // SAFETY: the console's input handle, and a buffer of `records.len()` records for the
        // call to fill, `n` of them filled.
        if unsafe { ReadConsoleInputW(input, records.as_mut_ptr(), records.len() as u32, &mut n) } == 0 {
            break;
        }
        for r in records.get(..n as usize).unwrap_or_default() {
            if u32::from(r.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: a key event's record holds a KEY_EVENT_RECORD.
            let key = unsafe { r.Event.KeyEvent };
            // SAFETY: either view of the character is a u16's bits.
            let unit = unsafe { key.uChar.UnicodeChar };
            // An answer is ASCII.
            if key.bKeyDown != 0
                && let Ok(byte) = u8::try_from(unit)
            {
                heard.push(byte);
            }
        }
        // DA1's answer, `ESC [ ? … c`, ends what there is to hear.
        if let Some(at) = find(&heard, b"\x1b[?")
            && heard.get(at..).is_some_and(|rest| rest.contains(&b'c'))
        {
            break;
        }
    }
    // SAFETY: the console's own handle, its mode given back.
    unsafe { SetConsoleMode(input, was) };
    parse_background(&heard)
}

/// Whether this process's stdout (`fd` 1) or stderr (2) is a console that takes escape
/// sequences: its virtual terminal processing turned on, as the Console's documentation
/// has a program ask for it (SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING); a console
/// that refuses it (Windows before 10 1511) takes none.
#[cfg(windows)]
pub fn styles(fd: i32) -> bool {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, SetConsoleMode,
    };
    let Some(h) = console(fd) else {
        return false;
    };
    let mut mode = 0;
    // SAFETY: the console's own handle, its mode read then set with one flag more.
    unsafe {
        GetConsoleMode(h, &mut mode) != 0
            && (mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0)
    }
}

/// The size of the console `fd` is, rows then columns, or 0×0 if it is not one: its
/// window's (GetConsoleScreenBufferInfo, as moby/term's GetWinsize reads it on Windows).
#[cfg(windows)]
pub fn size(fd: i32) -> (u16, u16) {
    use windows_sys::Win32::System::Console::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo};
    let Some(h) = console(fd) else {
        return (0, 0);
    };
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: the console's own handle, and a structure for the call to fill.
    if unsafe { GetConsoleScreenBufferInfo(h, &mut info) } == 0 {
        return (0, 0);
    }
    let w = info.srWindow;
    let span = |a: i16, b: i16| u16::try_from(i32::from(b) - i32::from(a) + 1).unwrap_or(0);
    (span(w.Top, w.Bottom), span(w.Left, w.Right))
}

/// Whether the console `fd` shows its whole screen buffer: a pseudo console's does
/// (ConPTY: Windows Terminal, an ssh session, an editor's terminal), whose buffer is its
/// size, and a buffer-size record ([`ConsoleInput`]) comes with each resize. A console
/// window with lines kept above it (conhost's scrollback) resizes its window within the
/// buffer, and says nothing of a resize that keeps the buffer's size
/// (WINDOW_BUFFER_SIZE_RECORD: "a change in the size of the console screen buffer").
#[cfg(windows)]
pub fn shows_whole_buffer(fd: i32) -> bool {
    use windows_sys::Win32::System::Console::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo};
    let Some(h) = console(fd) else {
        return false;
    };
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: the console's own handle, and a structure for the call to fill.
    if unsafe { GetConsoleScreenBufferInfo(h, &mut info) } == 0 {
        return false;
    }
    let w = info.srWindow;
    let span = |a: i16, b: i16| i32::from(b) - i32::from(a) + 1;
    span(w.Top, w.Bottom) == i32::from(info.dwSize.Y) && span(w.Left, w.Right) == i32::from(info.dwSize.X)
}

/// What reads stdin's console for a command's terminal: its input records
/// (ReadConsoleInputW), where `io::stdin()` reads text (ReadConsoleW). Text alone leaves
/// out the console's records of its screen buffer's size, which ReadConsole always filters
/// (SetConsoleMode), and Rust's reader takes a read that ends in ^Z for the end of input
/// (std sys/stdio/windows.rs, `read_u16s`): in a terminal of the command's, ^Z is a key it
/// takes, as ^C is. Keys give the text they make, in UTF-8, a terminal's escape sequences
/// among it where the console gives them (ENABLE_VIRTUAL_TERMINAL_INPUT); each record of
/// the buffer's size calls `resized`.
#[cfg(windows)]
pub struct ConsoleInput<F: FnMut()> {
    handle: windows_sys::Win32::Foundation::HANDLE,
    /// What the records read gave, not yet taken.
    text: Vec<u8>,
    /// A high surrogate whose low half has not come yet.
    high: Option<u16>,
    resized: F,
}

#[cfg(windows)]
impl<F: FnMut()> std::fmt::Debug for ConsoleInput<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleInput")
            .field("text", &self.text.len())
            .finish()
    }
}

/// The handle `ConsoleInput` reads is the process's console, there for its life.
// SAFETY: a console input handle may be read from any thread.
#[cfg(windows)]
unsafe impl<F: FnMut() + Send> Send for ConsoleInput<F> {}

#[cfg(windows)]
impl<F: FnMut()> ConsoleInput<F> {
    /// stdin's console's records, or `None` where stdin is no console.
    pub fn new(resized: F) -> Option<ConsoleInput<F>> {
        Some(ConsoleInput {
            handle: console_in()?,
            text: Vec::new(),
            high: None,
            resized,
        })
    }

    /// Adds a UTF-16 unit's text: a surrogate pair's once both halves have come; a half
    /// alone is U+FFFD, as a console's text is decoded elsewhere.
    fn unit(&mut self, unit: u16) {
        let mut put = |c: char| {
            let mut b = [0u8; 4];
            self.text.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        };
        match (self.high.take(), unit) {
            (None, 0xD800..=0xDBFF) => self.high = Some(unit),
            (Some(high), 0xDC00..=0xDFFF) => put(char::decode_utf16([high, unit])
                .next()
                .and_then(Result::ok)
                .unwrap_or('\u{FFFD}')),
            (high, unit) => {
                if high.is_some() {
                    put('\u{FFFD}');
                }
                match unit {
                    0xD800..=0xDBFF => self.high = Some(unit),
                    _ => put(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}')),
                }
            }
        }
    }
}

#[cfg(windows)]
impl<F: FnMut()> io::Read for ConsoleInput<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::System::Console::{
            INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, WINDOW_BUFFER_SIZE_EVENT,
        };
        // Alt released after a code typed on the number pad carries the character made
        // (crossterm src/event/sys/windows/parse.rs, `is_alt_code`); other releases none.
        const VK_MENU: u16 = 0x12;
        while self.text.is_empty() {
            let mut records = [INPUT_RECORD::default(); 64];
            let mut n = 0u32;
            // SAFETY: the console's input handle, and a buffer of `records.len()` records
            // for the call to fill, `n` of them filled.
            if unsafe { ReadConsoleInputW(self.handle, records.as_mut_ptr(), records.len() as u32, &mut n) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            for r in records.get(..n as usize).unwrap_or_default() {
                match u32::from(r.EventType) {
                    KEY_EVENT => {
                        // SAFETY: a key event's record holds a KEY_EVENT_RECORD.
                        let key = unsafe { r.Event.KeyEvent };
                        // SAFETY: either view of the character is a u16's bits.
                        let unit = unsafe { key.uChar.UnicodeChar };
                        let typed = key.bKeyDown != 0 || key.wVirtualKeyCode == VK_MENU;
                        if unit != 0 && typed {
                            for _ in 0..key.wRepeatCount.max(1) {
                                self.unit(unit);
                            }
                        }
                    }
                    WINDOW_BUFFER_SIZE_EVENT => (self.resized)(),
                    _ => {}
                }
            }
        }
        let n = buf.len().min(self.text.len());
        if let (Some(to), Some(from)) = (buf.get_mut(..n), self.text.get(..n)) {
            to.copy_from_slice(from);
        }
        self.text.drain(..n);
        Ok(n)
    }
}

/// The paint for stderr, if it is a colour terminal (a build's live progress; the look's
/// errors on Unix): a terminal that takes escape sequences, where `NO_COLOR` is unset and
/// `TERM` is not `dumb`.
pub(crate) fn styled_err() -> Option<shards_tui::tokens::Paint> {
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
    let env = |k: &str| std::env::var(k).ok();
    (color && styles(2)).then(|| shards_tui::tokens::Paint::new(shards_tui::tokens_truecolor(&env)))
}

/// The size of the terminal stderr is, rows then columns; 0×0 where it is none.
pub(crate) fn err_size() -> (usize, usize) {
    let (r, c) = size(2);
    (usize::from(r), usize::from(c))
}

#[cfg(unix)]
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

#[cfg(all(test, unix))]
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// A console goes raw: no echo, line editing, processed input or mouse events; its
    /// window events, insert and quick-edit mode, with the extended flags; and the rest of
    /// its mode as it was.
    #[test]
    fn a_console_goes_raw_and_keeps_the_rest_of_its_mode() {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_INSERT_MODE, ENABLE_LINE_INPUT,
            ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE,
            ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_WINDOW_INPUT,
        };
        let cooked = ENABLE_PROCESSED_INPUT
            | ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_MOUSE_INPUT
            | ENABLE_WINDOW_INPUT
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let raw = raw_mode(cooked);
        let gone = ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_MOUSE_INPUT;
        let set = ENABLE_WINDOW_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_INSERT_MODE | ENABLE_QUICK_EDIT_MODE;
        assert_eq!(raw & gone, 0);
        assert_eq!(raw & set, set);
        assert_eq!(
            raw & ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_INPUT,
            "the rest kept"
        );
        assert_eq!(
            raw_mode(0) & ENABLE_WINDOW_INPUT,
            ENABLE_WINDOW_INPUT,
            "window events asked for"
        );
    }

    /// What a command's terminal is given of a pseudo console (ConPTY, as Windows Terminal
    /// and ssh host one): each key's text as typed, ^Z and ^C among it, an arrow's escape
    /// sequence, text past the BMP; and each resize, as its record of the buffer's size,
    /// with the size it leaves, whole. The console is this test's own (CreatePseudoConsole),
    /// read by a copy of this test binary in it ([`console_child`]).
    #[test]
    fn a_pseudo_consoles_keys_and_resizes_reach_the_command() {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Console::{
            COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
        };
        use windows_sys::Win32::System::Pipes::CreatePipe;
        use windows_sys::Win32::System::Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
            EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
            STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
        };
        let dir = shards_testdir::TempDir::new("conpty").unwrap();
        let out = dir.join("got");
        let pipe = || {
            let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
            // SAFETY: two handles for the call to fill.
            assert_ne!(unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) }, 0);
            // SAFETY: handles just made, each owned here alone.
            unsafe { (OwnedHandle::from_raw_handle(r), OwnedHandle::from_raw_handle(w)) }
        };
        use std::os::windows::io::AsRawHandle as _;
        let (in_read, in_write) = pipe();
        let (out_read, out_write) = pipe();
        let mut pc: HPCON = 0;
        let size = |cols: i16, rows: i16| COORD { X: cols, Y: rows };
        // SAFETY: the pipes' ends the pseudo console takes copies of, and a handle to fill.
        let made = unsafe {
            CreatePseudoConsole(
                size(80, 25),
                in_read.as_raw_handle(),
                out_write.as_raw_handle(),
                0,
                &mut pc,
            )
        };
        assert_eq!(made, 0, "CreatePseudoConsole: {made:#x}");
        drop((in_read, out_write));
        // What the console draws, drained as it comes: one left full stops the child.
        let mut drawn = std::fs::File::from(out_read);
        let drain = std::thread::spawn(move || {
            let _ = std::io::copy(&mut drawn, &mut std::io::sink());
        });
        // The child: this binary's console_child, its environment this one's with the
        // file it writes what it read to.
        let exe = std::env::current_exe().unwrap();
        let mut line: Vec<u16> = format!(
            "\"{}\" cli::terminal::windows_tests::console_child --exact --test-threads 1",
            exe.display()
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let mut env: Vec<u16> = Vec::new();
        for (k, v) in std::env::vars_os().filter(|(k, _)| k != "SHARDS_CONSOLE_CHILD") {
            let pair = format!("{}={}", k.to_string_lossy(), v.to_string_lossy());
            env.extend(pair.encode_utf16().chain([0]));
        }
        env.extend(
            format!("SHARDS_CONSOLE_CHILD={}", out.display())
                .encode_utf16()
                .chain([0, 0]),
        );
        let mut bytes = 0usize;
        // SAFETY: asking the size of a list of one attribute.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
        let mut list = vec![0u8; bytes];
        let attrs: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
        // SAFETY: a buffer of the size asked, for one attribute.
        let listed = unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut bytes) };
        assert_ne!(listed, 0);
        // SAFETY: the pseudo console's handle, its size given, set on the list just made.
        let attributed = unsafe {
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                pc as *const std::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert_ne!(attributed, 0);
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs;
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: a command line and environment block of this test's, NUL-ended, and the
        // structures for the call to read and fill.
        let started = unsafe {
            CreateProcessW(
                std::ptr::null(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        assert_ne!(started, 0, "CreateProcessW: {}", io::Error::last_os_error());
        // SAFETY: the handles CreateProcessW made, owned here.
        let (process, _thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(pi.hProcess),
                OwnedHandle::from_raw_handle(pi.hThread),
            )
        };
        // SAFETY: the list made above, no longer needed once the process is made.
        unsafe { DeleteProcThreadAttributeList(attrs) };
        // Keys only once its console is raw, as a user types into a raw terminal.
        let ready = out.with_extension("ready");
        let t0 = std::time::Instant::now();
        while !ready.exists() {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(60),
                "the child never got ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut keys = std::fs::File::from(in_write);
        keys.write_all("ab\u{1a}\u{3}é\u{1F600}\x1b[A".as_bytes())
            .unwrap();
        // SAFETY: the pseudo console made above.
        assert_eq!(unsafe { ResizePseudoConsole(pc, size(100, 30)) }, 0);
        // The console takes the resize apart from the keys: the rest waits for the child
        // to have seen it, which the keys before it may have reached the child after.
        let resized = out.with_extension("resized");
        let t0 = std::time::Instant::now();
        while !resized.exists() {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(60),
                "no resize reached the child"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        keys.write_all(b"z.").unwrap();
        // SAFETY: the process made above, waited on for a minute at most.
        let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), 60_000) };
        assert_eq!(waited, WAIT_OBJECT_0, "the child did not end");
        let mut code = 1u32;
        // SAFETY: the process made above, ended, and its code read into a local.
        unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) };
        // SAFETY: the pseudo console made above, its child ended.
        unsafe { ClosePseudoConsole(pc) };
        drop(keys);
        let _ = drain.join();
        let got = std::fs::read_to_string(&out).unwrap_or_default();
        assert_eq!(code, 0, "the child failed: {got}");
        assert_eq!(
            got,
            format!(
                "{:?}\nsizes [(25, 80), (30, 100)] whole true\n",
                "ab\u{1a}\u{3}é\u{1F600}\x1b[Az."
            )
        );
    }

    /// The child of `a_pseudo_consoles_keys_and_resizes_reach_the_command`, in its pseudo
    /// console: its console raw, it reads keys until a full stop, and writes what it read
    /// and the sizes each resize left. Nothing where SHARDS_CONSOLE_CHILD is not set.
    #[test]
    fn console_child() {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::Console::{
            STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        };
        let Some(out) = std::env::var_os("SHARDS_CONSOLE_CHILD") else {
            return;
        };
        let out = std::path::PathBuf::from(out);
        // Its standard handles the console's, as a user's process in a terminal has them,
        // whatever the test's own were.
        let open = |name: &str| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(name)
                .unwrap()
        };
        let (conin, conout) = (open("CONIN$"), open("CONOUT$"));
        // SAFETY: handles of this process's console, held open for its life.
        unsafe {
            SetStdHandle(STD_INPUT_HANDLE, conin.as_raw_handle());
            SetStdHandle(STD_OUTPUT_HANDLE, conout.as_raw_handle());
            SetStdHandle(STD_ERROR_HANDLE, conout.as_raw_handle());
        }
        make_raw().unwrap();
        // Each size a resize left, said once: the parent waits for the second.
        let sizes = std::cell::RefCell::new(vec![size(1)]);
        let mut input = ConsoleInput::new(|| {
            let now = size(1);
            let mut sizes = sizes.borrow_mut();
            if sizes.last() != Some(&now) {
                sizes.push(now);
                std::fs::write(out.with_extension("resized"), b"").unwrap();
            }
        })
        .unwrap();
        std::fs::write(out.with_extension("ready"), b"").unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while !got.ends_with(b".") {
            let n = input.read(&mut buf).unwrap();
            assert_ne!(n, 0, "stdin ended after {got:?}");
            got.extend_from_slice(&buf[..n]);
        }
        let whole = shows_whole_buffer(1);
        drop(input);
        restore();
        let sizes = sizes.into_inner();
        std::fs::write(
            &out,
            format!(
                "{:?}\nsizes {sizes:?} whole {whole}\n",
                String::from_utf8_lossy(&got)
            ),
        )
        .unwrap();
        drop((conin, conout));
    }
}
