//! What a pipe's caller holds in flight one way (platform-11): the quota each end of a
//! Windows pipe of shards' is made with (CreateNamedPipeW's `nOutBufferSize` and
//! `nInBufferSize`), from its caller's own protocol, never a number of the pipe's.
//!
//! - A caller whose protocol writes frames of a bound size asks for the longest
//!   ([`Window::frame`]): a writer of one then writes it whole without waiting on its reader
//!   mid-frame, where a write past the quota "will block until the data is read from the
//!   pipe" (CreateNamedPipeW, Remarks).
//! - A caller whose flow has no frame of its own, bytes as a program writes them, asks for
//!   the system's ([`Window::System`]): the quota the system gives a pipe asked for none,
//!   read from one it makes so (CreatePipe's `nSize` of zero, for which "the system uses the
//!   default buffer size"; anonymous pipes "are implemented using a named pipe with a unique
//!   name", CreatePipe, Remarks), as Unix's own pipes and socket pairs hold the kernel's
//!   default for the same flows. CreateNamedPipeW documents no default of its own for a size
//!   of zero, so the pipe is asked for what CreatePipe was given.
//!
//! The quota is advisory: the system reserves "the system default, the system minimum or
//! maximum, or the specified size rounded up" (CreateNamedPipeW, Remarks), and a write past
//! what it reserved waits on its reader rather than fails, so a frame past a host's maximum
//! is still carried whole, only waited on. A window the call cannot ask for, a frame of no
//! bytes or one past a DWORD, is refused, typed ([`WindowRefusal`]).

use std::num::NonZeroU32;

/// What a pipe's caller holds in flight one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The longest frame its protocol writes whole, in bytes.
    Frame(NonZeroU32),
    /// A flow with no frame of its own: the system's default buffer.
    System,
}

/// Why a frame is no window a pipe can be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRefusal {
    /// A frame of no bytes: no protocol's.
    Empty,
    /// A frame past what a DWORD counts, which CreateNamedPipeW's quota is.
    PastDword { bytes: usize },
}

impl std::fmt::Display for WindowRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowRefusal::Empty => f.write_str("a pipe's window of a frame of no bytes"),
            WindowRefusal::PastDword { bytes } => {
                write!(f, "a pipe's window of a {bytes}-byte frame, past a DWORD's")
            }
        }
    }
}

impl std::error::Error for WindowRefusal {}

impl From<WindowRefusal> for std::io::Error {
    fn from(refusal: WindowRefusal) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, refusal)
    }
}

impl Window {
    /// The window of a protocol whose longest frame is `bytes` long.
    pub fn frame(bytes: usize) -> Result<Window, WindowRefusal> {
        let dword = u32::try_from(bytes).map_err(|_| WindowRefusal::PastDword { bytes })?;
        NonZeroU32::new(dword)
            .map(Window::Frame)
            .ok_or(WindowRefusal::Empty)
    }

    /// The quota a frame asks for each way, its bytes; none for the system's, which only the
    /// system says (`pipe::system_quota` on Windows).
    pub fn frame_bytes(self) -> Option<u32> {
        match self {
            Window::Frame(bytes) => Some(bytes.get()),
            Window::System => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A pipe's quota is its caller's window (platform-11): a frame's bytes asked for as they
    /// are, the system's left to the system; a frame of none or past a DWORD refused, typed.
    #[test]
    fn a_pipes_quota_is_its_callers_window() {
        for bytes in [1, 16, 4 + 1_048_656, 6 * 1024 * 1024 + 72, u32::MAX as usize] {
            let window = Window::frame(bytes).unwrap();
            assert_eq!(window.frame_bytes(), u32::try_from(bytes).ok(), "{window:?}");
        }
        assert_eq!(Window::System.frame_bytes(), None);
        assert_eq!(Window::frame(0), Err(WindowRefusal::Empty));
        let past = u32::MAX as usize + 1;
        assert_eq!(Window::frame(past), Err(WindowRefusal::PastDword { bytes: past }));
        let refused: std::io::Error = WindowRefusal::Empty.into();
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
    }
}
