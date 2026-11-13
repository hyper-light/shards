//! Why a vCPU's guest time was not read (guest-cpu-time-v1, R04): the cause its backend met,
//! typed, with the code its hypervisor or its OS gave, carried as it is to every reader (the
//! VM process's `GUEST_TIMED` answer, the daemon's stats, an orchestrator's, D231). Its words
//! are for showing alone.

use core::fmt;

/// Why a sample did not read a vCPU's guest time: the most it read before stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// Its vCPU has gone: the thread KVM ran it on has ended.
    Gone,
    /// HVF's `hv_vcpus_exit`, which takes the vCPU out of its guest to read it, failed: its
    /// `hv_return_t`.
    HvfExit(i32),
    /// HVF's `hv_vcpu_get_exec_time` failed as the vCPU last left its guest: its
    /// `hv_return_t`.
    HvfExecTime(i32),
    /// WHP's `WHvGetVirtualProcessorCounters` failed: its HRESULT.
    WhpCounters(i32),
    /// Linux's stat of the vCPU's thread (/proc/self/task/TID/stat) was not read: its errno,
    /// or none for a line Linux wrote that this does not parse.
    ThreadStat(Option<i32>),
}

impl Unread {
    /// Its cause's name, as its text form and the daemon's stats give it.
    pub fn cause(&self) -> &'static str {
        match self {
            Self::Gone => "gone",
            Self::HvfExit(_) => "hvf-exit",
            Self::HvfExecTime(_) => "hvf-exec-time",
            Self::WhpCounters(_) => "whp-counters",
            Self::ThreadStat(_) => "thread-stat",
        }
    }

    /// The code its hypervisor or its OS gave, where it gave one.
    pub fn code(&self) -> Option<i32> {
        match *self {
            Self::Gone | Self::ThreadStat(None) => None,
            Self::HvfExit(c) | Self::HvfExecTime(c) | Self::WhpCounters(c) | Self::ThreadStat(Some(c)) => Some(c),
        }
    }

    /// Its text form: its cause, then its code in decimal where it has one, or `malformed`
    /// for a stat line not parsed. [`parse`](Self::parse) reads it back.
    pub fn write(&self, out: &mut impl fmt::Write) -> fmt::Result {
        out.write_str(self.cause())?;
        match (self, self.code()) {
            (_, Some(code)) => write!(out, " {code}"),
            (Self::ThreadStat(None), None) => out.write_str(" malformed"),
            _ => Ok(()),
        }
    }

    /// What [`write`](Self::write) wrote, all of it; none for anything else.
    pub fn parse(text: &str) -> Option<Unread> {
        let mut words = text.split(' ');
        let cause = words.next()?;
        let code = words.next();
        if words.next().is_some() {
            return None;
        }
        let number = |c: Option<&str>| -> Option<i32> {
            // Decimal alone: `i32`'s parse takes a leading `+`, which `write` never writes.
            let c = c?;
            if c.starts_with('+') {
                return None;
            }
            c.parse().ok()
        };
        match cause {
            "gone" => code.is_none().then_some(Self::Gone),
            "hvf-exit" => number(code).map(Self::HvfExit),
            "hvf-exec-time" => number(code).map(Self::HvfExecTime),
            "whp-counters" => number(code).map(Self::WhpCounters),
            "thread-stat" => match code {
                Some("malformed") => Some(Self::ThreadStat(None)),
                other => number(other).map(|c| Self::ThreadStat(Some(c))),
            },
            _ => None,
        }
    }
}

impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A status's bits as the hypervisor documents them (hv_return_t, HRESULT): unsigned hex.
        let hex = |c: i32| u32::from_ne_bytes(c.to_ne_bytes());
        match *self {
            Self::Gone => f.write_str("its vCPU has gone"),
            Self::HvfExit(c) => write!(f, "hv_vcpus_exit failed: {:#010x}", hex(c)),
            Self::HvfExecTime(c) => write!(f, "hv_vcpu_get_exec_time failed: {:#010x}", hex(c)),
            Self::WhpCounters(c) => write!(f, "WHvGetVirtualProcessorCounters failed: HRESULT {:#010x}", hex(c)),
            Self::ThreadStat(Some(e)) => write!(f, "its thread's stat: errno {e}"),
            Self::ThreadStat(None) => f.write_str("its thread's stat: a line this does not parse"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Unread;
    use alloc::string::String;

    const EVERY: [Unread; 7] = [
        Unread::Gone,
        Unread::HvfExit(-85377023),
        Unread::HvfExecTime(0),
        Unread::WhpCounters(i32::MIN),
        Unread::WhpCounters(-2147024891),
        Unread::ThreadStat(Some(3)),
        Unread::ThreadStat(None),
    ];

    /// Each cause written reads back as itself, its code whole.
    #[test]
    fn each_cause_reads_back_as_itself() {
        for u in EVERY {
            let mut text = String::new();
            u.write(&mut text).unwrap();
            assert_eq!(Unread::parse(&text), Some(u), "{text:?}");
        }
        let mut text = String::new();
        Unread::WhpCounters(-2147024891).write(&mut text).unwrap();
        assert_eq!(text, "whp-counters -2147024891");
    }

    /// Anything else is none: a cause unknown, a code missing, extra, signed `+`, not decimal or
    /// past an `i32`, words not single-spaced.
    #[test]
    fn text_not_written_so_is_refused() {
        for text in [
            "",
            "lost",
            "gone 1",
            "hvf-exit",
            "hvf-exit +1",
            "hvf-exit 0x10",
            "hvf-exit 2147483648",
            "whp-counters 1 2",
            "whp-counters  1",
            "thread-stat",
            "thread-stat malformed 1",
            "its thread's stat: gone",
        ] {
            assert_eq!(Unread::parse(text), None, "{text:?}");
        }
    }

    /// Its words name the call and its code as the hypervisor documents codes: unsigned hex.
    #[test]
    fn its_words_name_the_call_and_its_code() {
        assert_eq!(
            alloc::format!("{}", Unread::WhpCounters(-2147024891)),
            "WHvGetVirtualProcessorCounters failed: HRESULT 0x80070005"
        );
        assert_eq!(Unread::ThreadStat(Some(3)).code(), Some(3));
        assert_eq!(Unread::ThreadStat(None).code(), None);
        assert_eq!(Unread::Gone.cause(), "gone");
    }
}
