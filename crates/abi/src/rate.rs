//! A managed machine's rate (D255) and use (D260): the line its host's client writes on the
//! machine's vsock socket in place of Firecracker's `CONNECT <port>`, and what its VM process
//! answers, by which an orchestrator holds the machine's vCPUs to a CPU rate with D191's
//! governor, as an image run's host does over its own link (D247), and reads what they and
//! the VM process use, as an image run's stats carry it (D241). Only a dial on that socket
//! reaches it: the guest's connections arrive as virtio-vsock packets, never as a host
//! client's line. One ask and its answer a connection.
//!
//! - The ask: `RATE <milli>`, the milli-CPU of the host's time the vCPUs may use together,
//!   0 for no hold.
//! - The answer: `RATED <milli> <period>`, what the vCPUs are held to (0 where they are let
//!   be) and the period the governor holds that over, in nanoseconds; or `REFUSED <why>`,
//!   in the VM process's words. One line.
//! - The ask: `USE`. The answer ([`used`]): `USED`, then the vCPUs' guest times as
//!   [`GuestTimes::encode`] writes them or `guest-unread <why>`, then the VM process's own
//!   use, `host <cpu_ns> <resident> <footprint>` or `host-unread <why>`, a line each, read in
//!   that order, so that the process's CPU time is never the less (D241). The VM process
//!   closes the connection after it.
//!
//! Words are matched without case, as Firecracker's muxer matches `CONNECT`
//! (firecracker `docs/vsock.md`), and a line may end `\r\n`, as a terminal ends one. The ask
//! is made and an answer read without allocating, as a managed guest's client asks it.

use alloc::string::String;
use core::fmt;
use core::fmt::Write as _;
use core::num::NonZeroU64;

use crate::guest_time::{GuestTimes, HostUse};

/// The ask's word.
pub const ASK: &str = "RATE";
/// The answer's word where the VM holds its vCPUs to the rate, or lets them be.
pub const HELD: &str = "RATED";
/// The answer's word where it holds them to nothing it was told.
pub const REFUSED: &str = "REFUSED";
/// The use ask's word (D260), and its whole line.
pub const USE: &str = "USE";
pub const USE_ASK: &[u8] = b"USE\n";
/// The use answer's word.
pub const USED: &str = "USED";

/// The longest ask: `RATE 18446744073709551615\r\n`, its rate a u64's most; [`USE_ASK`] is
/// shorter.
pub const ASK_MOST: usize = ASK.len() + 1 + decimal_digits(u64::MAX) + 2;
/// The longest answer: the VM process's words as long as the run protocol carries them, its
/// payload's bound (`run::MAX_PAYLOAD`), as an image run's `RATED` carries its VM's (D247).
/// A held answer, two u64s, is far shorter; so is a use answer of a machine's most vCPUs
/// (at most 4096, as `arch`'s tests hold both layouts to, a line each of at most 70 bytes,
/// D260).
pub const ANSWER_MOST: usize = crate::run::MAX_PAYLOAD as usize;

/// The decimal digits of `n`.
pub const fn decimal_digits(mut n: u64) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// An ask's line, made without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ask {
    bytes: [u8; ASK_MOST],
    len: usize,
}

impl Ask {
    /// Its line's bytes, its newline last.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

/// The ask for `milli` milli-CPU, 0 for no hold: `RATE <milli>\n`.
pub fn ask(milli: u64) -> Ask {
    let mut bytes = [0u8; ASK_MOST];
    let mut len = 0;
    let mut put = |b: u8| {
        if let Some(at) = bytes.get_mut(len) {
            *at = b;
            len += 1;
        }
    };
    ASK.bytes().for_each(&mut put);
    put(b' ');
    let mut digits = [0u8; decimal_digits(u64::MAX)];
    let mut n = milli;
    let mut first = digits.len();
    loop {
        first -= 1;
        if let Some(d) = digits.get_mut(first) {
            // `n % 10` is a digit: below 10, which a u8 holds.
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 || first == 0 {
            break;
        }
    }
    digits
        .get(first..)
        .unwrap_or_default()
        .iter()
        .copied()
        .for_each(&mut put);
    put(b'\n');
    Ask { bytes, len }
}

/// What a host client asks of a managed machine on its vsock socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// Its vCPUs held to this many milli-CPU, 0 for no hold (D255).
    Rate(u64),
    /// What its vCPUs have run of its guest and its VM process has used of its host (D260).
    Use,
}

/// What a whole ask's line asks for: `RATE`, one u64 and its newline, or `USE` and its
/// newline; nothing after.
pub fn read_ask(line: &[u8]) -> Option<Asked> {
    let mut words = line_words(line)?;
    let word = words.next()?;
    let asked = if word.eq_ignore_ascii_case(ASK) {
        Asked::Rate(words.next()?.parse().ok()?)
    } else if word.eq_ignore_ascii_case(USE) {
        Asked::Use
    } else {
        return None;
    };
    words.next().is_none().then_some(asked)
}

/// What the VM process said of a rate, its words borrowed from its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer<'a> {
    /// What its vCPUs are held to, none where they are let be (no rate, or every vCPU's
    /// whole CPU or more), and the period its governor holds over, in nanoseconds.
    Held {
        milli: Option<NonZeroU64>,
        period_ns: NonZeroU64,
    },
    /// Why it holds them to nothing it was told, in its words.
    Refused(&'a str),
}

/// Why an answer's line is none of this protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broken {
    /// Not UTF-8, or no line: no newline ends it.
    Text,
    /// Its first word is neither [`HELD`] nor [`REFUSED`].
    Word,
    /// A held answer's rate or period is no decimal u64, or one of them is missing.
    Number,
    /// A held answer's period is none: a governor holds over some time.
    Period,
    /// Words past a held answer's two numbers, or bytes past its line.
    Trailing,
}

impl fmt::Display for Broken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "no line of UTF-8 text",
            Self::Word => "neither RATED nor REFUSED",
            Self::Number => "a rate or a period that is no decimal u64",
            Self::Period => "a period of none",
            Self::Trailing => "words past its line's last",
        })
    }
}

/// The line of `answer`: a refusal's words on one line, each control character a space, cut
/// at a character's edge to fit [`ANSWER_MOST`].
pub fn answer(answer: &Answer<'_>) -> String {
    match answer {
        Answer::Held { milli, period_ns } => {
            alloc::format!("{HELD} {} {period_ns}\n", milli.map_or(0, NonZeroU64::get))
        }
        Answer::Refused(why) => {
            let mut line = String::with_capacity(ANSWER_MOST.min(REFUSED.len() + 2 + why.len()));
            line.push_str(REFUSED);
            line.push(' ');
            push_words(&mut line, why, ANSWER_MOST);
            line
        }
    }
}

/// `why` and its newline after `text`, on one line, each control character a space, cut at a
/// character's edge so that `text` stays within `most` bytes.
fn push_words(text: &mut String, why: &str, most: usize) {
    let most = most.saturating_sub(1);
    for c in why.chars() {
        if text.len() + c.len_utf8() > most {
            break;
        }
        text.push(if c.is_control() { ' ' } else { c });
    }
    text.push('\n');
}

/// The longest held `host` line: its key and three u64s, spaced, and its newline.
const HOST_MOST: usize = "host".len() + 3 * (1 + decimal_digits(u64::MAX)) + 1;

/// The use answer (D260): `USED`, the guest's times or why not, then the VM process's own use
/// or why not, a line each, within [`ANSWER_MOST`]: a guest's words cut to leave room for the
/// longest host line, a host's to what is left.
pub fn used(guest: Result<&GuestTimes, &str>, host: Result<&HostUse, &str>) -> String {
    let mut text = String::new();
    text.push_str(USED);
    text.push('\n');
    match guest {
        Ok(times) => text.push_str(&times.encode()),
        Err(why) => {
            text.push_str("guest-unread ");
            push_words(&mut text, why, ANSWER_MOST.saturating_sub(HOST_MOST));
        }
    }
    match host {
        Ok(u) => {
            let _ = writeln!(
                text,
                "host {} {} {}",
                u.cpu_ns, u.resident_bytes, u.footprint_bytes
            );
        }
        Err(why) => {
            text.push_str("host-unread ");
            push_words(&mut text, why, ANSWER_MOST);
        }
    }
    text
}

/// What a VM process said of its machine's use (D260), each part its reading or why not, in
/// its words borrowed from its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Used<'a> {
    pub guest: Result<GuestTimes, &'a str>,
    pub host: Result<HostUse, &'a str>,
}

/// Why a use answer is none of this protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseBroken {
    /// Past [`ANSWER_MOST`].
    Long,
    /// Not UTF-8, or its last line has no newline: its VM process ended before it was said.
    Text,
    /// Its first line is not [`USED`].
    Word,
    /// Its guest's part is neither times [`GuestTimes::decode`] reads nor one
    /// `guest-unread` line.
    Guest,
    /// Its last line is neither `host` and three u64s nor `host-unread`.
    Host,
}

impl fmt::Display for UseBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Long => "past the longest answer",
            Self::Text => "no whole lines of UTF-8 text",
            Self::Word => "no USED",
            Self::Guest => "a guest time none of the protocol's",
            Self::Host => "a host use none of the protocol's",
        })
    }
}

/// The use a whole answer says, as [`used`] writes it.
pub fn read_used(answer: &[u8]) -> Result<Used<'_>, UseBroken> {
    if answer.len() > ANSWER_MOST {
        return Err(UseBroken::Long);
    }
    let text = core::str::from_utf8(answer).map_err(|_| UseBroken::Text)?;
    let body = text.strip_suffix('\n').ok_or(UseBroken::Text)?;
    let (word, rest) = body.split_once('\n').ok_or(UseBroken::Word)?;
    if !word.trim().eq_ignore_ascii_case(USED) {
        return Err(UseBroken::Word);
    }
    let (guest, host) = rest.rsplit_once('\n').ok_or(UseBroken::Guest)?;
    let guest = match guest.strip_prefix("guest-unread ") {
        Some(why) if !why.contains('\n') => Err(why.trim()),
        Some(_) => return Err(UseBroken::Guest),
        None => Ok(GuestTimes::decode(guest).ok_or(UseBroken::Guest)?),
    };
    let host = match host.strip_prefix("host-unread ") {
        Some(why) => Err(why.trim()),
        None => {
            let mut words = host.split(' ');
            if words.next() != Some("host") {
                return Err(UseBroken::Host);
            }
            let mut number = || {
                words
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .ok_or(UseBroken::Host)
            };
            let u = HostUse {
                cpu_ns: number()?,
                resident_bytes: number()?,
                footprint_bytes: number()?,
            };
            if words.next().is_some() {
                return Err(UseBroken::Host);
            }
            Ok(u)
        }
    };
    Ok(Used { guest, host })
}

/// The answer a whole line says: a held one exactly its word and two u64s, a refusal its word
/// and the rest of its line, trimmed.
pub fn read_answer(line: &[u8]) -> Result<Answer<'_>, Broken> {
    let text = core::str::from_utf8(line).map_err(|_| Broken::Text)?;
    let (line, rest) = text.split_once('\n').ok_or(Broken::Text)?;
    if !rest.is_empty() {
        return Err(Broken::Trailing);
    }
    let line = line.strip_suffix('\r').unwrap_or(line).trim_start();
    let (word, said) = line.split_once(' ').unwrap_or((line.trim_end(), ""));
    if word.eq_ignore_ascii_case(REFUSED) {
        return Ok(Answer::Refused(said.trim()));
    }
    if !word.eq_ignore_ascii_case(HELD) {
        return Err(Broken::Word);
    }
    let mut numbers = said.split_whitespace();
    let mut number = || {
        numbers
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or(Broken::Number)
    };
    let milli = NonZeroU64::new(number()?);
    let period_ns = NonZeroU64::new(number()?).ok_or(Broken::Period)?;
    if numbers.next().is_some() {
        return Err(Broken::Trailing);
    }
    Ok(Answer::Held { milli, period_ns })
}

/// A whole line's words: UTF-8, ending in its one newline, `\r` before it allowed.
fn line_words(line: &[u8]) -> Option<core::str::SplitWhitespace<'_>> {
    let text = core::str::from_utf8(line).ok()?;
    let (line, rest) = text.split_once('\n')?;
    rest.is_empty().then(|| line.split_whitespace())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// An ask reads back as its rate; its word in any case, with or without a terminal's
    /// carriage return; anything else, none: no number, two, a sign, past a u64, a word
    /// other than its own, or bytes after its line (D255).
    #[test]
    fn an_ask_reads_back_as_its_rate_and_nothing_else_does() {
        for milli in [0, 1, 9, 10, 250, 1000, u64::MAX] {
            let line = ask(milli);
            assert_eq!(line.as_bytes(), alloc::format!("RATE {milli}\n").as_bytes());
            assert_eq!(read_ask(line.as_bytes()), Some(Asked::Rate(milli)));
        }
        assert_eq!(read_ask(b"rate 7\r\n"), Some(Asked::Rate(7)));
        assert_eq!(read_ask(b"RATE  7 \n"), Some(Asked::Rate(7)));
        for bad in [
            &b"RATE\n"[..],
            b"RATE x\n",
            b"RATE 1 2\n",
            b"RATE -1\n",
            b"RATE 1x\n",
            b"RATE 18446744073709551616\n",
            b"RATED 1\n",
            b"CONNECT 1\n",
            b"RATE 1",
            b"RATE 1\nrest",
            b"RATE \xff\n",
        ] {
            assert_eq!(read_ask(bad), None, "{bad:?}");
        }
        assert_eq!(read_ask(USE_ASK), Some(Asked::Use));
        assert_eq!(read_ask(b"use\r\n"), Some(Asked::Use));
        for bad in [&b"USE 1\n"[..], b"USED\n", b"USE", b"USE\nmore"] {
            assert_eq!(read_ask(bad), None, "{bad:?}");
        }
        assert!(USE_ASK.len() < ASK_MOST);
        assert_eq!(ASK_MOST, b"RATE 18446744073709551615\r\n".len());
        assert_eq!(
            read_ask(b"RATE 18446744073709551615\r\n"),
            Some(Asked::Rate(u64::MAX))
        );
    }

    /// An answer reads back as it was said: held to a rate or let be, with its period; a
    /// refusal in its words. A refusal's words stay on its line, every control character a
    /// space, cut at a character's edge to fit the longest answer.
    #[test]
    fn an_answer_reads_back_as_it_was_said() {
        let held = |milli, period| Answer::Held {
            milli: NonZeroU64::new(milli),
            period_ns: NonZeroU64::new(period).unwrap(),
        };
        for said in [
            held(250, 100_000_000),
            held(0, 1),
            held(u64::MAX, u64::MAX),
            Answer::Refused("the microVM has not started"),
            Answer::Refused(""),
        ] {
            let line = answer(&said);
            assert!(line.len() <= ANSWER_MOST);
            assert_eq!(read_answer(line.as_bytes()), Ok(said));
        }
        assert_eq!(answer(&held(250, 100)), "RATED 250 100\n");
        assert_eq!(answer(&held(0, 100)), "RATED 0 100\n");
        assert_eq!(
            answer(&Answer::Refused("two\nlines\r\tand a bell\x07")),
            "REFUSED two lines  and a bell \n"
        );
        let long = answer(&Answer::Refused(&"é".repeat(ANSWER_MOST)));
        assert!(long.len() <= ANSWER_MOST, "{}", long.len());
        assert!(long.ends_with("é\n"));
        assert_eq!(read_answer(b"rated 1 2\r\n"), Ok(held(1, 2)));
        assert_eq!(
            read_answer(b"refused  the hold's thread \n"),
            Ok(Answer::Refused("the hold's thread"))
        );
    }

    /// An answer none of the protocol's words make is refused by what breaks it, never read
    /// as a hold.
    #[test]
    fn a_broken_answer_is_refused_by_what_breaks_it() {
        for (line, broken) in [
            (&b"RATED 1 2"[..], Broken::Text),
            (b"RATED \xff 2\n", Broken::Text),
            (b"OK 7\n", Broken::Word),
            (b"\n", Broken::Word),
            (b"RATED\n", Broken::Number),
            (b"RATED 1\n", Broken::Number),
            (b"RATED x 2\n", Broken::Number),
            (b"RATED 1 18446744073709551616\n", Broken::Number),
            (b"RATED 1 0\n", Broken::Period),
            (b"RATED 1 2 3\n", Broken::Trailing),
            (b"RATED 1 2\nmore", Broken::Trailing),
        ] {
            assert_eq!(read_answer(line), Err(broken), "{line:?}");
        }
    }

    /// D260: a use answer reads back as it was said: the guest's times and the VM process's
    /// use, or either's words where it was not read, each on its own line; words cut to fit.
    #[test]
    fn a_use_answer_reads_back_as_it_was_said() {
        use crate::guest_time::{Unread, VcpuTime};
        let times = GuestTimes {
            vcpus: alloc::vec![
                VcpuTime {
                    index: 0,
                    guest_ns: 900,
                    unread: None,
                },
                VcpuTime {
                    index: 3,
                    guest_ns: u64::MAX,
                    unread: Some(Unread::HvfExecTime(-85377023)),
                },
            ],
            departed_ns: 7,
        };
        let host = HostUse {
            cpu_ns: 5_000,
            resident_bytes: u64::MAX,
            footprint_bytes: 0,
        };
        let text = used(Ok(&times), Ok(&host));
        assert_eq!(
            text,
            "USED\ndeparted 7\nvcpu 0 900\nvcpu 3 18446744073709551615 unread hvf-exec-time \
             -85377023\nhost 5000 18446744073709551615 0\n"
        );
        let read = read_used(text.as_bytes()).unwrap();
        assert_eq!(read.guest, Ok(times.clone()));
        assert_eq!(read.host, Ok(host));
        let none = used(Err("the microVM has not\nstarted"), Err("proc_pidinfo: denied"));
        assert_eq!(
            read_used(none.as_bytes()),
            Ok(Used {
                guest: Err("the microVM has not started"),
                host: Err("proc_pidinfo: denied"),
            })
        );
        let empty = GuestTimes::default();
        let read = used(Ok(&empty), Ok(&host));
        assert_eq!(read_used(read.as_bytes()).unwrap().guest, Ok(empty));
        let long = "é".repeat(ANSWER_MOST);
        let both = used(Err(&long), Err(&long));
        assert!(both.len() <= ANSWER_MOST, "{}", both.len());
        let read = read_used(both.as_bytes()).unwrap();
        assert!(read.guest.is_err() && read.host.is_err());
        assert_eq!(
            HOST_MOST,
            "host 18446744073709551615 18446744073709551615 18446744073709551615\n".len()
        );
    }

    /// D260: a use answer none of the protocol's words make is refused by what breaks it:
    /// cut short (its VM process ended as it answered), past the longest, its word, a guest's
    /// part or a host's line malformed.
    #[test]
    fn a_broken_use_answer_is_refused_by_what_breaks_it() {
        let past = alloc::vec![b'x'; ANSWER_MOST + 1];
        for (answer, broken) in [
            (&b""[..], UseBroken::Text),
            (b"USED\ndeparted 1\nhost 1 2 3", UseBroken::Text),
            (b"USED\xff\n", UseBroken::Text),
            (&past[..], UseBroken::Long),
            (b"USED\n", UseBroken::Word),
            (b"RATED\ndeparted 1\nhost 1 2 3\n", UseBroken::Word),
            (b"USED\nhost 1 2 3\n", UseBroken::Guest),
            (b"USED\nvcpu 0 1\nhost 1 2 3\n", UseBroken::Guest),
            (b"USED\nguest-unread a\nb\nhost 1 2 3\n", UseBroken::Guest),
            (b"USED\ndeparted 1\nhost 1 2\n", UseBroken::Host),
            (b"USED\ndeparted 1\nhost 1 2 3 4\n", UseBroken::Host),
            (b"USED\ndeparted 1\nhost -1 2 3\n", UseBroken::Host),
            (b"USED\ndeparted 1\nvcpu 0 1\n", UseBroken::Host),
        ] {
            assert_eq!(
                read_used(answer),
                Err(broken),
                "{:?}",
                core::str::from_utf8(answer)
            );
        }
    }
}
