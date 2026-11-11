//! Protected Linux-guest lifecycle DATA shared by init and its carried helper.
//! Decoding a PID, status or packet never authenticates a principal or mints rights.

use core::fmt;
use core::num::NonZeroI32;

pub const VERSION: u8 = 1;
// PID namespace init's first child; the actual clone result must agree before use.
pub const ACTOR_PID: i32 = 2;
pub const PACKET_BYTES: usize = 3 * size_of::<u8>() + size_of::<u32>() + size_of::<i32>();
pub const FAILURE_BYTES: usize = size_of::<u8>() + size_of::<i32>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Step {
    Helper = 1,
    Setup,
    Exec,
    MountClone,
    MountAttach,
    MountClose,
    ControlLost,
    ControlProtocol,
    SelectedPathLookup = 10,
    SelectedSourceStat,
    SelectedVisibleStat,
}

impl Step {
    pub fn decode(tag: u8) -> Option<Self> {
        Some(match tag {
            1 => Self::Helper,
            2 => Self::Setup,
            3 => Self::Exec,
            4 => Self::MountClone,
            5 => Self::MountAttach,
            6 => Self::MountClose,
            7 => Self::ControlLost,
            8 => Self::ControlProtocol,
            10 => Self::SelectedPathLookup,
            11 => Self::SelectedSourceStat,
            12 => Self::SelectedVisibleStat,
            _ => return None,
        })
    }
}

/// Checked native operation failure. Construction rejects zero/negative errno.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeFailure {
    operation: Step,
    errno: NonZeroI32,
}

impl NativeFailure {
    pub fn operation(self) -> Step {
        self.operation
    }

    pub fn errno(self) -> i32 {
        self.errno.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Native(NativeFailure),
    SelectedObjectMismatch,
}

// Local protocol identifier9 is a comparison result, never a native operation.
// Its four trailing bytes are canonical UNUSED zero padding, not errno zero.
const SELECTED_OBJECT_MISMATCH: u8 = 9;

impl Failure {
    pub fn native(operation: Step, errno: i32) -> Result<Self, Error> {
        if errno <= 0 {
            return Err(Error::Fields);
        }
        let errno = NonZeroI32::new(errno).ok_or(Error::Fields)?;
        Ok(Self::Native(NativeFailure { operation, errno }))
    }

    pub fn as_native(self) -> Option<NativeFailure> {
        match self {
            Self::Native(failure) => Some(failure),
            Self::SelectedObjectMismatch => None,
        }
    }

    /// Native tags1–3 keep the existing five bytes exactly. New comparison
    /// tag9 has four UNUSED bytes; selected native operations use tags10–12.
    pub fn encode(self) -> [u8; FAILURE_BYTES] {
        match self {
            Self::Native(failure) => {
                let [a, b, c, d] = failure.errno().to_be_bytes();
                [failure.operation() as u8, a, b, c, d]
            }
            Self::SelectedObjectMismatch => [SELECTED_OBJECT_MISMATCH, 0, 0, 0, 0],
        }
    }

    pub fn decode(bytes: [u8; FAILURE_BYTES]) -> Option<Self> {
        let [tag, a, b, c, d] = bytes;
        if tag == SELECTED_OBJECT_MISMATCH {
            return ([a, b, c, d] == [0; size_of::<i32>()]).then_some(Self::SelectedObjectMismatch);
        }
        Self::native(Step::decode(tag)?, i32::from_be_bytes([a, b, c, d])).ok()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(failure) => write!(out, "{:?}: errno {}", failure.operation(), failure.errno()),
            Self::SelectedObjectMismatch => {
                out.write_str("selected object differs from final visible object")
            }
        }
    }
}
impl core::error::Error for Failure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    ActorBorn = 1,
    BaseReady,
    HelperBorn,
    HelperReady,
    Registered,
    Release,
    Released,
    ActorReaped,
    HelperReaped,
    Ack,
    Failed,
}

impl TryFrom<u8> for Kind {
    type Error = Error;
    fn try_from(tag: u8) -> Result<Self, Error> {
        Ok(match tag {
            1 => Self::ActorBorn,
            2 => Self::BaseReady,
            3 => Self::HelperBorn,
            4 => Self::HelperReady,
            5 => Self::Registered,
            6 => Self::Release,
            7 => Self::Released,
            8 => Self::ActorReaped,
            9 => Self::HelperReaped,
            10 => Self::Ack,
            11 => Self::Failed,
            _ => return Err(Error::Tag),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Version,
    Tag,
    Fields,
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length => out.write_str("invalid lifecycle packet length"),
            Self::Version => out.write_str("unsupported lifecycle packet version"),
            Self::Tag => out.write_str("unknown lifecycle packet tag"),
            Self::Fields => out.write_str("invalid lifecycle packet fields"),
        }
    }
}
impl core::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    Control { kind: Kind, pid: i32, detail: i32 },
    Failed { pid: i32, failure: Failure },
}

impl Packet {
    pub fn checked(kind: Kind, pid: i32, detail: i32) -> Result<Self, Error> {
        let packet = Self::Control { kind, pid, detail };
        packet.validate()?;
        Ok(packet)
    }

    pub fn failed(pid: i32, failure: Failure) -> Result<Self, Error> {
        let packet = Self::Failed { pid, failure };
        packet.validate()?;
        Ok(packet)
    }

    pub fn kind(self) -> Kind {
        match self {
            Self::Control { kind, .. } => kind,
            Self::Failed { .. } => Kind::Failed,
        }
    }

    pub fn pid(self) -> i32 {
        match self {
            Self::Control { pid, .. } | Self::Failed { pid, .. } => pid,
        }
    }

    pub fn detail(self) -> Option<i32> {
        match self {
            Self::Control { detail, .. } => Some(detail),
            Self::Failed { .. } => None,
        }
    }

    pub fn failure(self) -> Option<Failure> {
        match self {
            Self::Failed { failure, .. } => Some(failure),
            Self::Control { .. } => None,
        }
    }

    fn validate(self) -> Result<(), Error> {
        let Self::Control { kind, pid, detail } = self else {
            return if self.pid() >= ACTOR_PID {
                Ok(())
            } else {
                Err(Error::Fields)
            };
        };
        let valid = match kind {
            Kind::ActorBorn | Kind::Released => pid == ACTOR_PID && detail == 0,
            Kind::BaseReady => pid == 0 && detail == 0,
            Kind::HelperBorn | Kind::HelperReady | Kind::Registered => pid > ACTOR_PID && detail == 0,
            Kind::Release => pid == ACTOR_PID && matches!(detail, 0 | 1),
            // This DATA codec preserves kernel wait encoding, not proof of wait4.
            Kind::ActorReaped => pid == ACTOR_PID && detail >= 0,
            Kind::HelperReaped => pid > ACTOR_PID && detail >= 0,
            Kind::Failed => false,
            Kind::Ack => {
                pid >= ACTOR_PID
                    && u8::try_from(detail)
                        .ok()
                        .and_then(|v| Kind::try_from(v).ok())
                        .is_some_and(|kind| {
                            matches!(
                                kind,
                                Kind::ActorBorn
                                    | Kind::HelperBorn
                                    | Kind::HelperReady
                                    | Kind::Release
                                    | Kind::ActorReaped
                                    | Kind::HelperReaped
                            )
                        })
            }
        };
        if valid { Ok(()) } else { Err(Error::Fields) }
    }

    pub fn has_descriptor(self) -> bool {
        matches!(self.kind(), Kind::ActorBorn | Kind::HelperBorn)
    }

    pub fn ack(self) -> Result<Self, Error> {
        self.validate()?;
        Self::checked(Kind::Ack, self.pid(), i32::from(self.kind() as u8))
    }

    pub fn matches_ack(self, ack: Self) -> bool {
        ack.kind() == Kind::Ack
            && ack.pid() == self.pid()
            && ack.detail() == Some(i32::from(self.kind() as u8))
    }

    pub fn encode(self) -> Result<[u8; PACKET_BYTES], Error> {
        self.validate()?;
        let [p, q, r, s] = u32::try_from(self.pid())
            .map_err(|_| Error::Fields)?
            .to_be_bytes();
        let [a, b, c, d, tag] = match self {
            Self::Control { detail, .. } => {
                let [a, b, c, d] = detail.to_be_bytes();
                [a, b, c, d, 0]
            }
            Self::Failed { failure, .. } => {
                // Keep existing11B native packet positions; this rotation carries
                // exactly the SAME canonical five-byte Failure, never split fields.
                let [tag, a, b, c, d] = failure.encode();
                [a, b, c, d, tag]
            }
        };
        Ok([VERSION, self.kind() as u8, p, q, r, s, a, b, c, d, tag])
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let &[version, kind, p, q, r, s, a, b, c, d, tag] = bytes else {
            return Err(Error::Length);
        };
        if version != VERSION {
            return Err(Error::Version);
        }
        let kind = kind.try_into()?;
        let pid = i32::try_from(u32::from_be_bytes([p, q, r, s])).map_err(|_| Error::Fields)?;
        if kind == Kind::Failed {
            Self::failed(pid, Failure::decode([tag, a, b, c, d]).ok_or(Error::Fields)?)
        } else if tag != 0 {
            Err(Error::Fields)
        } else {
            Self::checked(kind, pid, i32::from_be_bytes([a, b, c, d]))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    extern crate alloc;
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn fixed_packets_preserve_steps_and_reject_every_partial_or_extra_record() {
        let packets = [
            Packet::checked(Kind::ActorBorn, ACTOR_PID, 0).unwrap(),
            Packet::checked(Kind::BaseReady, 0, 0).unwrap(),
            Packet::checked(Kind::HelperBorn, 3, 0).unwrap(),
            Packet::checked(Kind::HelperReady, i32::MAX, 0).unwrap(),
            Packet::checked(Kind::Registered, 3, 0).unwrap(),
            Packet::checked(Kind::Release, ACTOR_PID, 1).unwrap(),
            Packet::checked(Kind::Released, ACTOR_PID, 0).unwrap(),
            Packet::checked(Kind::ActorReaped, ACTOR_PID, 0).unwrap(),
            Packet::checked(Kind::HelperReaped, 3, i32::MAX).unwrap(),
        ];
        for packet in packets {
            let bytes = packet.encode().unwrap();
            assert_eq!(bytes.len(), PACKET_BYTES);
            assert_eq!(Packet::decode(&bytes), Ok(packet));
            for cut in 0..bytes.len() {
                assert_eq!(Packet::decode(&bytes[..cut]), Err(Error::Length));
            }
            let mut extra = Vec::from(bytes);
            extra.push(0);
            assert_eq!(Packet::decode(&extra), Err(Error::Length));
            let mut invalid = bytes;
            invalid[0] = VERSION + 1;
            assert_eq!(Packet::decode(&invalid), Err(Error::Version));
            invalid = bytes;
            invalid[1] = u8::MAX;
            assert_eq!(Packet::decode(&invalid), Err(Error::Tag));
            invalid = bytes;
            invalid[PACKET_BYTES - 1] = Step::Setup as u8;
            assert_eq!(Packet::decode(&invalid), Err(Error::Fields));
        }
        for tag in 1..=8 {
            let step = Step::decode(tag).unwrap();
            let failure = Failure::native(step, i32::MAX).unwrap();
            assert_eq!(Failure::decode(failure.encode()), Some(failure));
            let packet = Packet::failed(3, failure).unwrap();
            assert_eq!(Packet::decode(&packet.encode().unwrap()), Ok(packet));
            assert_eq!(packet.failure(), Some(failure));
        }
        assert_eq!(Failure::decode([1, 0, 0, 0, 0]), None);
        assert_eq!(Failure::decode([u8::MAX, 0, 0, 0, 1]), None);
    }

    #[test]
    fn acknowledgement_identity_and_descriptor_shape_are_exact() {
        let actor = Packet::checked(Kind::ActorBorn, ACTOR_PID, 0).unwrap();
        let helper = Packet::checked(Kind::HelperBorn, 3, 0).unwrap();
        let ready = Packet::checked(Kind::HelperReady, 3, 0).unwrap();
        assert!(actor.has_descriptor() && helper.has_descriptor());
        assert!(!ready.has_descriptor());
        assert!(helper.matches_ack(helper.ack().unwrap()));
        assert!(!helper.matches_ack(actor.ack().unwrap()));
        assert!(!helper.matches_ack(ready.ack().unwrap()));
        for (kind, pid, detail) in [
            (Kind::ActorBorn, 3, 0),
            (Kind::HelperBorn, ACTOR_PID, 0),
            (Kind::BaseReady, ACTOR_PID, 0),
            (Kind::Release, ACTOR_PID, 2),
            (Kind::Ack, 3, i32::from(Kind::BaseReady as u8)),
            (Kind::HelperReaped, 3, -1),
            (Kind::Failed, 3, 1),
        ] {
            assert_eq!(Packet::checked(kind, pid, detail), Err(Error::Fields));
        }
    }

    #[test]
    fn legacy_native_status_and_packet_bytes_are_unchanged() {
        for (step, tag) in [(Step::Helper, 1), (Step::Setup, 2), (Step::Exec, 3)] {
            for errno in [1, 2, 13, 0x01020304, i32::MAX] {
                let failure = Failure::native(step, errno).unwrap();
                let [a, b, c, d] = errno.to_be_bytes();
                assert_eq!(failure.encode(), [tag, a, b, c, d]);
                let packet = Packet::failed(ACTOR_PID, failure).unwrap();
                assert_eq!(
                    packet.encode().unwrap(),
                    [VERSION, Kind::Failed as u8, 0, 0, 0, 2, a, b, c, d, tag]
                );
                assert_eq!(Failure::decode([tag, a, b, c, d]), Some(failure));
            }
        }
    }

    #[test]
    fn mismatch_is_not_errno_and_padding_and_native_operations_are_canonical() {
        let mismatch = Failure::SelectedObjectMismatch;
        assert_eq!(mismatch.as_native(), None);
        assert_eq!(mismatch.encode(), [9, 0, 0, 0, 0]);
        assert_eq!(Failure::decode(mismatch.encode()), Some(mismatch));
        let packet = Packet::failed(ACTOR_PID, mismatch).unwrap();
        assert_eq!(packet.detail(), None);
        assert_eq!(packet.failure(), Some(mismatch));
        assert_eq!(Packet::decode(&packet.encode().unwrap()), Ok(packet));
        for position in 1..FAILURE_BYTES {
            let mut noncanonical = mismatch.encode();
            noncanonical[position] = 1;
            assert_eq!(Failure::decode(noncanonical), None);
        }
        for (operation, tag) in [
            (Step::SelectedPathLookup, 10),
            (Step::SelectedSourceStat, 11),
            (Step::SelectedVisibleStat, 12),
        ] {
            let failure = Failure::native(operation, 13).unwrap();
            assert_eq!(failure.encode(), [tag, 0, 0, 0, 13]);
            let native = failure.as_native().unwrap();
            assert_eq!(native.operation(), operation);
            assert_eq!(native.errno(), 13);
            assert_eq!(Failure::decode(failure.encode()), Some(failure));
            for errno in [0, -1, i32::MIN] {
                assert_eq!(Failure::native(operation, errno), Err(Error::Fields));
                let [a, b, c, d] = errno.to_be_bytes();
                assert_eq!(Failure::decode([tag, a, b, c, d]), None);
            }
        }
        for tag in [0, 13, u8::MAX] {
            assert_eq!(Failure::decode([tag, 0, 0, 0, 1]), None);
        }
        assert_eq!(Step::decode(9), None);
        let invalid = Packet::Control {
            kind: Kind::Failed,
            pid: ACTOR_PID,
            detail: 13,
        };
        assert_eq!(invalid.encode(), Err(Error::Fields));
        let mut invalid_padding = packet.encode().unwrap();
        invalid_padding[6] = 1;
        assert_eq!(Packet::decode(&invalid_padding), Err(Error::Fields));
    }
}
