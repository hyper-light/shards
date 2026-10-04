//! How shards looks on a colour terminal: hyperlight's design (hyperlight-site), drawn
//! with text. A quiet near-black page, prismatic colour only where it counts, motion
//! that is slow, continuous and subordinate to what matters, and the shards mark.
//!
//! - `tokens`: the site's colours, its prism, and painting them on 24-bit or 256-colour
//!   terminals.
//! - `motion`: one clock per display and the site's curves: expo-out, smoothstep, the
//!   exponential follow, travelling glints, breaths.
//! - `canvas`: Braille cells as a 2×4 dot canvas, for drawing at sub-cell resolution.
//! - `mark`: the shards mark, its three shards swaying, their colours cycling, a glint
//!   travelling each rim.
//! - `bar`: hairline progress bars, their leading cell lit by how far into it they are.
//! - `rate`: throughput, smoothed, with its recent history as a sparkline.
//! - `text`: eyebrows in spaced capitals, sizes, rates and times as Docker prints them.
//! - `layout`: rows that fit their width by changing shape, not by being cut.
//! - `frame`: drawing a frame over the last, in place, whatever the terminal's width did.
//! - `panel`: errors, in a hairline panel.
//!
//! Nothing here allocates per frame once a display has grown to its size, and nothing
//! reads the clock but [`motion::Clock`]: a frame is a function of time, which tests set.

pub mod bar;
pub mod canvas;
pub mod frame;
pub mod layout;
pub mod mark;
pub mod motion;
pub mod panel;
pub mod rate;
pub mod text;
pub mod tokens;

/// Whether the terminal says it shows 24-bit colour (`COLORTERM`), as most do: the rest
/// get xterm's 256.
pub fn tokens_truecolor(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var("COLORTERM").is_some_and(|v| v == "truecolor" || v == "24bit")
}
