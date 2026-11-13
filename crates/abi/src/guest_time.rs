//! A microVM's vCPUs' guest time as a sample read it, and its text, one form for every reader
//! (the VM process's `GUEST_TIMED` answer to its daemon, a managed machine's `USED` to its
//! host's client, D260); and why a vCPU's was not read (guest-cpu-time-v1, R04): the cause its
//! backend met, typed, with the code its hypervisor or its OS gave, carried as it is to every
//! reader (the daemon's stats, an orchestrator's, D231). Its words are for showing alone.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::fmt::Write as _;

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
            Self::HvfExit(c) | Self::HvfExecTime(c) | Self::WhpCounters(c) | Self::ThreadStat(Some(c)) => {
                Some(c)
            }
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
            Self::WhpCounters(c) => write!(
                f,
                "WHvGetVirtualProcessorCounters failed: HRESULT {:#010x}",
                hex(c)
            ),
            Self::ThreadStat(Some(e)) => write!(f, "its thread's stat: errno {e}"),
            Self::ThreadStat(None) => f.write_str("its thread's stat: a line this does not parse"),
        }
    }
}

/// What a VM process uses of its host, as the host counts it (`platform::process_usage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostUse {
    /// Its CPU time, user and system together, in ns: its vCPUs' guest time among it.
    pub cpu_ns: u64,
    pub resident_bytes: u64,
    /// Its own memory, as memory per VM is counted (PM M30): macOS's physical footprint,
    /// Linux's anonymous resident pages, Windows' private working set.
    pub footprint_bytes: u64,
}

/// One vCPU's guest time, as a sample read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuTime {
    pub index: u32,
    /// Nanoseconds, never less than an earlier sample of the same vCPU said.
    pub guest_ns: u64,
    /// Why this sample did not read it, typed: `guest_ns` is then the most it read before.
    pub unread: Option<Unread>,
}

/// A machine's vCPUs' guest time, as one sample read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestTimes {
    /// The vCPUs it has, by index.
    pub vcpus: Vec<VcpuTime>,
    /// What the vCPUs it had and no longer has ran of their guest: still its machine's.
    pub departed_ns: u64,
}

impl GuestTimes {
    /// Its machine's: what every vCPU it has had ran of its guest.
    pub fn total_ns(&self) -> u64 {
        self.vcpus
            .iter()
            .fold(self.departed_ns, |total, v| total.saturating_add(v.guest_ns))
    }

    /// As text, a line each, in decimal: `departed NS`, then `vcpu INDEX NS` for each vCPU,
    /// ` unread CAUSE [CODE]` after where this sample did not read it ([`Unread::write`]), as a
    /// VM process answers its daemon.
    pub fn encode(&self) -> String {
        let mut text = alloc::format!("departed {}\n", self.departed_ns);
        for v in &self.vcpus {
            let _ = write!(text, "vcpu {} {}", v.index, v.guest_ns);
            if let Some(why) = &v.unread {
                text.push_str(" unread ");
                let _ = why.write(&mut text);
            }
            text.push('\n');
        }
        text
    }

    /// What [`encode`](Self::encode) wrote; `None` for anything else: a line of its keys
    /// malformed, its departed time missing or said twice, or its vCPUs out of index
    /// order. Lines of other keys are passed over, for a later VM's to come.
    pub fn decode(text: &str) -> Option<GuestTimes> {
        let mut departed = None;
        let mut vcpus: Vec<VcpuTime> = Vec::new();
        for line in text.lines() {
            let mut words = line.splitn(5, ' ');
            match words.next() {
                Some("departed") => {
                    let ns = words.next()?.parse().ok()?;
                    if departed.replace(ns).is_some() || words.next().is_some() {
                        return None;
                    }
                }
                Some("vcpu") => {
                    let index = words.next()?.parse().ok()?;
                    let guest_ns = words.next()?.parse().ok()?;
                    let unread = match (words.next(), words.next()) {
                        (None, _) => None,
                        (Some("unread"), Some(cause)) => Some(Unread::parse(cause)?),
                        _ => return None,
                    };
                    if vcpus.last().is_some_and(|v| v.index >= index) {
                        return None;
                    }
                    // Fallible, as a managed guest's client reads its VM's answer (D260).
                    vcpus.try_reserve(1).ok()?;
                    vcpus.push(VcpuTime {
                        index,
                        guest_ns,
                        unread,
                    });
                }
                _ => {}
            }
        }
        Some(GuestTimes {
            vcpus,
            departed_ns: departed?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{GuestTimes, Unread, VcpuTime};
    use alloc::string::String;
    use alloc::vec;

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

    /// Times encoded decode as they were: a vCPU unread, its cause and code whole; one unread
    /// with no code; none; and the most a u64 holds.
    #[test]
    fn encoded_times_decode_as_they_were() {
        let t = GuestTimes {
            vcpus: vec![
                VcpuTime {
                    index: 0,
                    guest_ns: 123_456_789,
                    unread: None,
                },
                VcpuTime {
                    index: 5,
                    guest_ns: u64::MAX,
                    unread: Some(Unread::WhpCounters(-2147024891)),
                },
                VcpuTime {
                    index: 6,
                    guest_ns: 0,
                    unread: Some(Unread::ThreadStat(None)),
                },
            ],
            departed_ns: 99,
        };
        let text = t.encode();
        assert_eq!(
            text,
            "departed 99\nvcpu 0 123456789\nvcpu 5 18446744073709551615 unread whp-counters \
             -2147024891\nvcpu 6 0 unread thread-stat malformed\n"
        );
        assert_eq!(GuestTimes::decode(&text), Some(t.clone()));
        assert_eq!(t.total_ns(), u64::MAX, "the total saturates");
        let none = GuestTimes::default();
        assert_eq!(GuestTimes::decode(&none.encode()), Some(none));
    }

    /// Text that is not encoded times is refused; lines of keys a later VM may add are not.
    #[test]
    fn text_other_than_encoded_times_is_refused() {
        for bad in [
            "",
            "vcpu 0 1\n",
            "departed\n",
            "departed x\n",
            "departed 1 2\n",
            "departed 1\ndeparted 2\n",
            "departed 1\nvcpu 0\n",
            "departed 1\nvcpu 0 -1\n",
            "departed 1\nvcpu 0 1 lost\n",
            "departed 1\nvcpu 0 1 unread\n",
            "departed 1\nvcpu 0 1 unread its thread's stat: gone\n",
            "departed 1\nvcpu 0 1 unread whp-counters\n",
            "departed 1\nvcpu 1 5\nvcpu 0 5\n",
            "departed 1\nvcpu 0 5\nvcpu 0 6\n",
        ] {
            assert_eq!(GuestTimes::decode(bad), None, "{bad:?}");
        }
        let later = GuestTimes::decode("departed 4\nsteal 9\nvcpu 1 6\n").unwrap();
        assert_eq!((later.total_ns(), later.vcpus.len()), (10, 1));
    }
}
