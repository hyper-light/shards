//! A refused run cut, correlated with its existing private restore tokens (D176).

use core::fmt;

pub const REQUEST_BYTES: usize = 2 * super::TOKEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Reason {
    Starting = 1,
    Stopping,
    Draining,
    Quarantined,
    Faulted,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Stopping => "stopping",
            Self::Draining => "draining",
            Self::Quarantined => "quarantined with unknown population",
            Self::Faulted => "management boundary fault",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Length { actual: usize },
    UnknownReason(u8),
    InvalidErrno { reason: Reason, errno: i32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { actual } => {
                write!(f, "quiesce refusal is {actual} bytes; expected {}", Refused::LEN)
            }
            Self::UnknownReason(reason) => write!(f, "unknown quiesce refusal reason {reason}"),
            Self::InvalidErrno { reason, errno } => {
                write!(f, "invalid errno {errno} for quiesce refusal {reason}")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    request: [u8; REQUEST_BYTES],
    pub member: u32,
    pub reason: Reason,
    pub errno: i32,
}

impl fmt::Debug for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuiesceRefused")
            .field("member", &self.member)
            .field("reason", &self.reason)
            .field("errno", &self.errno)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tenant {} is {}", self.member, self.reason)?;
        if self.errno != 0 {
            write!(f, "; errno {}", self.errno)?;
        }
        Ok(())
    }
}

impl Refused {
    pub const LEN: usize = REQUEST_BYTES + size_of::<u32>() + size_of::<u8>() + size_of::<i32>();

    pub fn new(request: [u8; REQUEST_BYTES], member: u32, reason: Reason, errno: i32) -> Option<Self> {
        let valid = match reason {
            Reason::Quarantined | Reason::Faulted => errno > 0,
            _ => errno == 0,
        };
        valid.then_some(Self {
            request,
            member,
            reason,
            errno,
        })
    }

    pub fn matches(&self, request: &[u8; REQUEST_BYTES]) -> bool {
        &self.request == request
    }

    pub fn encode(self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        for (to, from) in out.iter_mut().zip(
            self.request
                .into_iter()
                .chain(self.member.to_be_bytes())
                .chain([self.reason as u8])
                .chain(self.errno.to_be_bytes()),
        ) {
            *to = from;
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let length = DecodeError::Length { actual: bytes.len() };
        let (request, rest) = bytes.split_first_chunk::<REQUEST_BYTES>().ok_or(length)?;
        let (member, rest) = rest.split_first_chunk::<{ size_of::<u32>() }>().ok_or(length)?;
        let (&reason, rest) = rest.split_first().ok_or(length)?;
        let errno = i32::from_be_bytes(rest.try_into().map_err(|_| length)?);
        let reason = match reason {
            1 => Reason::Starting,
            2 => Reason::Stopping,
            3 => Reason::Draining,
            4 => Reason::Quarantined,
            5 => Reason::Faulted,
            other => return Err(DecodeError::UnknownReason(other)),
        };
        Self::new(*request, u32::from_be_bytes(*member), reason, errno)
            .ok_or(DecodeError::InvalidErrno { reason, errno })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use alloc::format;

    #[test]
    fn a_refusal_is_fixed_typed_and_bound_to_its_request() {
        let token = [0x5a; REQUEST_BYTES];
        let refused = Refused::new(token, 7, Reason::Quarantined, 21).unwrap();
        let bytes = refused.encode();
        assert_eq!(Refused::decode(&bytes), Ok(refused));
        assert!(refused.matches(&token));
        assert!(!refused.matches(&[0x5b; REQUEST_BYTES]));
        for cut in 0..bytes.len() {
            assert!(Refused::decode(&bytes[..cut]).is_err());
        }
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert!(Refused::decode(&longer).is_err());
        for unknown in [0, 6, u8::MAX] {
            let mut bad = bytes;
            bad[REQUEST_BYTES + size_of::<u32>()] = unknown;
            assert_eq!(Refused::decode(&bad), Err(DecodeError::UnknownReason(unknown)));
        }
        assert_eq!(
            format!("{refused}"),
            "tenant 7 is quarantined with unknown population; errno 21"
        );
        assert!(!format!("{refused:?}").contains("90, 90"));
    }

    #[test]
    fn a_lifecycle_reason_cannot_disguise_an_invalid_error() {
        for reason in [Reason::Quarantined, Reason::Faulted] {
            for errno in [i32::MIN, -1, 0] {
                assert!(Refused::new([0; REQUEST_BYTES], 0, reason, errno).is_none());
            }
        }
        for reason in [Reason::Starting, Reason::Stopping, Reason::Draining] {
            assert!(Refused::new([0; REQUEST_BYTES], 0, reason, 1).is_none());
            let word = Refused::new([0; REQUEST_BYTES], 0, reason, 0).unwrap().encode();
            assert_eq!(Refused::decode(&word).unwrap().reason, reason);
        }
    }
}
