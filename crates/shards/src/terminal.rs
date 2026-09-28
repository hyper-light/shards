//! Raw terminal mode for interactive guest consoles: keystrokes reach the guest unchanged
//! (no echo, line editing or signal keys) and the guest's escape sequences render.

/// The terminal in raw mode; dropping it restores the previous mode.
#[derive(Debug)]
pub struct RawTerminal(imp::Saved);

impl RawTerminal {
    /// `None` when stdin is not an interactive terminal or its mode cannot change.
    pub fn enable() -> Option<RawTerminal> {
        imp::enable().map(RawTerminal)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        imp::restore(&self.0);
    }
}

#[cfg(unix)]
mod imp {
    pub struct Saved(libc::termios);

    impl std::fmt::Debug for Saved {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Saved(termios)")
        }
    }

    pub fn enable() -> Option<Saved> {
        // SAFETY: termios calls on fd 0 with a zero-initialized struct they fill in.
        unsafe {
            if libc::isatty(0) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            (libc::tcsetattr(0, libc::TCSANOW, &raw) == 0).then_some(Saved(saved))
        }
    }

    pub fn restore(saved: &Saved) {
        // SAFETY: restores attributes previously read from the same descriptor.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved.0) };
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    #[derive(Debug)]
    pub struct Saved {
        input: CONSOLE_MODE,
        /// Unset when stdout is not a console (e.g. redirected to a file).
        output: Option<CONSOLE_MODE>,
    }

    fn console(which: u32) -> Option<(HANDLE, CONSOLE_MODE)> {
        // SAFETY: GetStdHandle and GetConsoleMode only read process state; a non-console
        // handle makes GetConsoleMode fail.
        unsafe {
            let h = GetStdHandle(which);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut mode = 0;
            (GetConsoleMode(h, &mut mode) != 0).then_some((h, mode))
        }
    }

    /// Console input delivers keys as VT sequences with no local processing, and console
    /// output interprets the guest's VT sequences ("Console Virtual Terminal Sequences",
    /// Windows Console documentation).
    pub fn enable() -> Option<Saved> {
        let (input, in_mode) = console(STD_INPUT_HANDLE)?;
        let raw = (in_mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        // SAFETY: sets the mode of a console input handle this process owns.
        if unsafe { SetConsoleMode(input, raw) } == 0 {
            return None;
        }
        let output = console(STD_OUTPUT_HANDLE).and_then(|(h, mode)| {
            // SAFETY: as above, for the console output handle.
            let ok = unsafe { SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) } != 0;
            ok.then_some(mode)
        });
        Some(Saved {
            input: in_mode,
            output,
        })
    }

    pub fn restore(saved: &Saved) {
        // SAFETY: restores modes previously read from the same console handles.
        unsafe {
            if let Some((h, _)) = console(STD_INPUT_HANDLE) {
                SetConsoleMode(h, saved.input);
            }
            if let (Some(mode), Some((h, _))) = (saved.output, console(STD_OUTPUT_HANDLE)) {
                SetConsoleMode(h, mode);
            }
        }
    }
}
