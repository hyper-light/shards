//! What a device step (`kind::DEVICE`, D214) comes to, as the VM process answers it
//! (`kind::DEVICED`) and its daemon or run's host reads it: its answer, or why not, by
//! cause, with what each cause names (devices-10, R04). Text is made of a refusal only to
//! show it.

use std::collections::TryReserveError;

/// Why a device step was not done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The microVM was made with no PCI slots to plug a device into.
    NoBus,
    /// Every one of its `slots` hot-plug slots holds a function.
    NoSlots { slots: u64 },
    /// No function is plugged at `devfn`.
    NoDevice { devfn: u8 },
    /// Its guest's driver still drives the function at `devfn`, as `reason` says.
    StillDriven { devfn: u8, reason: String },
    /// Its disk's file is `asked` bytes, shorter than the `has` its guest was told: a disk
    /// only grows under its guest.
    Shrink { has: u64, asked: u64 },
    /// Its disk's file is `bytes` long, not whole sectors of `sector`.
    Unaligned { bytes: u64, sector: u64 },
    /// The VM process has no running microVM to do it in.
    Inactive,
    /// The step itself is malformed, or names what this VM process does not take.
    InvalidRequest(String),
    /// What the device is cannot do it: a join disk grows only as it is given files.
    Unsupported(String),
    /// The host failed it: a file, a share's stream, a function's VFIO.
    Host(String),
    /// Its microVM ended, or its link did, before it answered: the step may have been done.
    ConnectionEnded,
    /// Its answer could not be read: the step may have been done.
    InvalidReply,
}

/// What a refusal leaves its asker to do before asking again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// Ask again once what holds it lets go: a slot frees, its guest lets the device go.
    AfterRelease,
    /// Asked again as it is, it is refused again.
    CorrectRequest,
    /// The step may have been done: find out what the microVM has before asking again.
    Reconcile,
    /// The host's failure says nothing of whether it lasts.
    Unknown,
}

impl Refusal {
    pub fn retry(&self) -> Retry {
        match self {
            Self::NoSlots { .. } | Self::StillDriven { .. } => Retry::AfterRelease,
            Self::Host(_) => Retry::Unknown,
            Self::ConnectionEnded | Self::InvalidReply => Retry::Reconcile,
            Self::NoBus
            | Self::NoDevice { .. }
            | Self::Shrink { .. }
            | Self::Unaligned { .. }
            | Self::Inactive
            | Self::InvalidRequest(_)
            | Self::Unsupported(_) => Retry::CorrectRequest,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBus => f.write_str("this microVM has no PCI slots to plug a device into"),
            Self::NoSlots { slots } => write!(f, "every one of its {slots} hot-plug slots holds a function"),
            Self::NoDevice { devfn } => write!(
                f,
                "no function is plugged at 0000:00:{:02x}.{}",
                devfn >> 3,
                devfn & 7
            ),
            Self::StillDriven { reason, .. } => f.write_str(reason),
            Self::Shrink { has, asked } => write!(
                f,
                "its file shrank to {asked} bytes from {has}: a disk only grows under its guest"
            ),
            Self::Unaligned { bytes, sector } => {
                write!(f, "its file of {bytes} bytes: not whole sectors of {sector}")
            }
            Self::Inactive => f.write_str("the microVM has not started"),
            Self::InvalidRequest(why) | Self::Unsupported(why) | Self::Host(why) => f.write_str(why),
            Self::ConnectionEnded => {
                f.write_str("it stopped before it answered: the step may have been done")
            }
            Self::InvalidReply => {
                f.write_str("an answer that could not be read: the step may have been done")
            }
        }
    }
}

/// Text shown of a refusal, where what asked for it only says why.
impl From<Refusal> for String {
    fn from(refusal: Refusal) -> String {
        refusal.to_string()
    }
}

/// An answer's first byte, after its number.
const DONE: u8 = 0;
const REFUSED: u8 = 1;

/// A refusal's cause, its second byte; its fields follow, big-endian, then its text.
mod cause {
    pub const NO_BUS: u8 = 0;
    pub const NO_SLOTS: u8 = 1;
    pub const NO_DEVICE: u8 = 2;
    pub const STILL_DRIVEN: u8 = 3;
    pub const SHRINK: u8 = 4;
    pub const UNALIGNED: u8 = 5;
    pub const INACTIVE: u8 = 6;
    pub const INVALID_REQUEST: u8 = 7;
    pub const UNSUPPORTED: u8 = 8;
    pub const HOST: u8 = 9;
    pub const CONNECTION_ENDED: u8 = 10;
    pub const INVALID_REPLY: u8 = 11;
}

/// `answer` as `DEVICED` carries it after its number: [`DONE`] and its text, or [`REFUSED`],
/// its cause, its fields and its text.
pub fn encode(answer: &Result<String, Refusal>) -> Result<Vec<u8>, TryReserveError> {
    let mut head = [0u8; 2 + 2 * 8];
    let (fixed, text): (usize, &str) = match answer {
        Ok(text) => (1, text),
        Err(refusal) => {
            head[0] = REFUSED;
            let mut put = |at: usize, v: u64| {
                if let Some(to) = head.get_mut(at..at + 8) {
                    to.copy_from_slice(&v.to_be_bytes());
                }
            };
            let (cause, len, text) = match refusal {
                Refusal::NoBus => (cause::NO_BUS, 2, ""),
                Refusal::NoSlots { slots } => {
                    put(2, *slots);
                    (cause::NO_SLOTS, 10, "")
                }
                Refusal::NoDevice { devfn } => {
                    put(2, u64::from(*devfn));
                    (cause::NO_DEVICE, 10, "")
                }
                Refusal::StillDriven { devfn, reason } => {
                    put(2, u64::from(*devfn));
                    (cause::STILL_DRIVEN, 10, reason.as_str())
                }
                Refusal::Shrink { has, asked } => {
                    put(2, *has);
                    put(10, *asked);
                    (cause::SHRINK, 18, "")
                }
                Refusal::Unaligned { bytes, sector } => {
                    put(2, *bytes);
                    put(10, *sector);
                    (cause::UNALIGNED, 18, "")
                }
                Refusal::Inactive => (cause::INACTIVE, 2, ""),
                Refusal::InvalidRequest(why) => (cause::INVALID_REQUEST, 2, why.as_str()),
                Refusal::Unsupported(why) => (cause::UNSUPPORTED, 2, why.as_str()),
                Refusal::Host(why) => (cause::HOST, 2, why.as_str()),
                Refusal::ConnectionEnded => (cause::CONNECTION_ENDED, 2, ""),
                Refusal::InvalidReply => (cause::INVALID_REPLY, 2, ""),
            };
            head[1] = cause;
            (len, text)
        }
    };
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(fixed.saturating_add(text.len()))?;
    bytes.extend_from_slice(head.get(..fixed).unwrap_or_default());
    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
}

/// What [`encode`] made, read back; [`Refusal::InvalidReply`] for anything it does not make.
pub fn decode(bytes: &[u8]) -> Result<String, Refusal> {
    let (&status, rest) = bytes.split_first().ok_or(Refusal::InvalidReply)?;
    match status {
        DONE => text(rest),
        REFUSED => Err(refusal(rest)?),
        _ => Err(Refusal::InvalidReply),
    }
}

fn refusal(bytes: &[u8]) -> Result<Refusal, Refusal> {
    let (&which, mut rest) = bytes.split_first().ok_or(Refusal::InvalidReply)?;
    let mut field = || -> Result<u64, Refusal> {
        let (v, after) = rest.split_first_chunk::<8>().ok_or(Refusal::InvalidReply)?;
        rest = after;
        Ok(u64::from_be_bytes(*v))
    };
    let devfn = |v: u64| u8::try_from(v).map_err(|_| Refusal::InvalidReply);
    let refusal = match which {
        cause::NO_BUS => Refusal::NoBus,
        cause::NO_SLOTS => Refusal::NoSlots { slots: field()? },
        cause::NO_DEVICE => Refusal::NoDevice {
            devfn: devfn(field()?)?,
        },
        cause::STILL_DRIVEN => {
            let devfn = devfn(field()?)?;
            return Ok(Refusal::StillDriven {
                devfn,
                reason: text(rest)?,
            });
        }
        cause::SHRINK => Refusal::Shrink {
            has: field()?,
            asked: field()?,
        },
        cause::UNALIGNED => Refusal::Unaligned {
            bytes: field()?,
            sector: field()?,
        },
        cause::INACTIVE => Refusal::Inactive,
        cause::INVALID_REQUEST => return Ok(Refusal::InvalidRequest(text(rest)?)),
        cause::UNSUPPORTED => return Ok(Refusal::Unsupported(text(rest)?)),
        cause::HOST => return Ok(Refusal::Host(text(rest)?)),
        cause::CONNECTION_ENDED => Refusal::ConnectionEnded,
        cause::INVALID_REPLY => Refusal::InvalidReply,
        _ => return Err(Refusal::InvalidReply),
    };
    // A cause without text has nothing after its fields.
    if rest.is_empty() {
        Ok(refusal)
    } else {
        Err(Refusal::InvalidReply)
    }
}

fn text(bytes: &[u8]) -> Result<String, Refusal> {
    let text = std::str::from_utf8(bytes).map_err(|_| Refusal::InvalidReply)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| Refusal::InvalidReply)?;
    owned.push_str(text);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_refusals_keep_identity_and_retry_across_the_wire() {
        for refusal in [
            Refusal::NoBus,
            Refusal::NoSlots { slots: 4 },
            Refusal::NoDevice { devfn: 0x10 },
            Refusal::StillDriven {
                devfn: 8,
                reason: "driver active".into(),
            },
            Refusal::Shrink {
                has: 1024,
                asked: 512,
            },
            Refusal::Unaligned {
                bytes: 513,
                sector: 512,
            },
            Refusal::Inactive,
            Refusal::InvalidRequest("a disk of no file".into()),
            Refusal::Unsupported("a join disk grows as it is given files".into()),
            Refusal::Host("VFIO map failed".into()),
            Refusal::ConnectionEnded,
            Refusal::InvalidReply,
        ] {
            let expected_retry = refusal.retry();
            let decoded = decode(&encode(&Err(refusal.clone())).unwrap()).unwrap_err();
            assert_eq!(decoded.retry(), expected_retry);
            assert_eq!(decoded, refusal);
        }
        assert_eq!(
            decode(&encode(&Ok("8 0000:00:01.0".into())).unwrap()),
            Ok("8 0000:00:01.0".into())
        );
        assert_eq!(decode(&encode(&Ok(String::new())).unwrap()), Ok(String::new()));
    }

    /// An answer cut short, of an unknown status or cause, of a field past its range, or
    /// of text where its cause has none, is one that may have been done, never a success
    /// or another cause.
    #[test]
    fn malformed_device_answers_are_uncertain_not_success() {
        let mut long_devfn = vec![REFUSED, cause::NO_DEVICE];
        long_devfn.extend_from_slice(&256u64.to_be_bytes());
        let mut trailing = vec![REFUSED, cause::NO_SLOTS];
        trailing.extend_from_slice(&4u64.to_be_bytes());
        trailing.push(b'x');
        for bytes in [
            &[][..],
            &[2],
            &[DONE, 0xff],
            &[REFUSED],
            &[REFUSED, 0xff],
            &[REFUSED, cause::NO_SLOTS, 0, 0],
            &[REFUSED, cause::SHRINK, 0, 0, 0, 0, 0, 0, 0, 1],
            &[REFUSED, cause::NO_BUS, b'x'],
            &long_devfn,
            &trailing,
        ] {
            assert_eq!(decode(bytes), Err(Refusal::InvalidReply), "{bytes:?}");
        }
    }
}
