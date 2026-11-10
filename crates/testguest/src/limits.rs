//! Fixed test-fixture records shared by the guest actor and its host observer.

/// Linux6.18 include/uapi/asm-generic/errno-base.h, used by both guest architectures.
/// The host's libc errno can differ (Darwin EAGAIN is not Linux EAGAIN).
pub const LINUX_EAGAIN: i32 = 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub tasks: u32,
    pub reserved: u32,
    pub children: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Budget,
    Length,
    Version,
    Phase,
    Cause,
    Command,
}

impl Budget {
    pub fn new(tasks: u32, reserved: u32, children: u32) -> Result<Self, Invalid> {
        if reserved == 0 || children == 0 || reserved.checked_add(children) != Some(tasks) {
            return Err(Invalid::Budget);
        }
        Ok(Self {
            tasks,
            reserved,
            children,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ready,
    Refused,
    Recovered,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Fill,
    Release,
    Finish,
}
impl Command {
    pub fn encode(self) -> [u8; 1] {
        [match self {
            Self::Fill => 1,
            Self::Release => 2,
            Self::Finish => 3,
        }]
    }
    pub fn decode(bytes: [u8; 1]) -> Result<Self, Invalid> {
        match bytes {
            [1] => Ok(Self::Fill),
            [2] => Ok(Self::Release),
            [3] => Ok(Self::Finish),
            _ => Err(Invalid::Command),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Cause {
    None = 0,
    ForkRefused = 1,
    Io = 2,
    Input = 3,
    Protocol = 4,
    UnexpectedAdmission = 5,
    WrongRefusal = 6,
    ChildStatus = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub phase: Phase,
    pub budget: Budget,
    pub actor: u32,
    pub parent: u32,
    pub uid: u32,
    pub reaper_uid: u32,
    pub threads: u32,
    pub held: u32,
    pub reaped: u32,
    pub replacement: u32,
    pub descriptors: u32,
    pub cause: Cause,
    pub errno: i32,
}

impl Report {
    // This fixture schema is sixteen network-order u32 words, including its version.
    pub const BYTES: usize = 64;

    pub fn encode(self) -> [u8; Self::BYTES] {
        let phase = match self.phase {
            Phase::Ready => 1,
            Phase::Refused => 2,
            Phase::Recovered => 3,
            Phase::Failed => 4,
        };
        let words = [
            1,
            phase,
            self.budget.tasks,
            self.budget.reserved,
            self.budget.children,
            self.actor,
            self.parent,
            self.uid,
            self.reaper_uid,
            self.threads,
            self.held,
            self.reaped,
            self.replacement,
            self.descriptors,
            self.cause as u32,
            u32::from_be_bytes(self.errno.to_be_bytes()),
        ];
        let mut bytes = [0; Self::BYTES];
        for (word, output) in words.into_iter().zip(bytes.chunks_exact_mut(4)) {
            output.copy_from_slice(&word.to_be_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Invalid> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().map_err(|_| Invalid::Length)?;
        let mut words = [0; 16];
        for (word, input) in words.iter_mut().zip(bytes.chunks_exact(4)) {
            *word = u32::from_be_bytes(input.try_into().map_err(|_| Invalid::Length)?);
        }
        let [
            version,
            phase,
            tasks,
            reserved,
            children,
            actor,
            parent,
            uid,
            reaper_uid,
            threads,
            held,
            reaped,
            replacement,
            descriptors,
            cause,
            errno,
        ] = words;
        if version != 1 {
            return Err(Invalid::Version);
        }
        let phase = match phase {
            1 => Phase::Ready,
            2 => Phase::Refused,
            3 => Phase::Recovered,
            4 => Phase::Failed,
            _ => return Err(Invalid::Phase),
        };
        let cause = match cause {
            0 => Cause::None,
            1 => Cause::ForkRefused,
            2 => Cause::Io,
            3 => Cause::Input,
            4 => Cause::Protocol,
            5 => Cause::UnexpectedAdmission,
            6 => Cause::WrongRefusal,
            7 => Cause::ChildStatus,
            _ => return Err(Invalid::Cause),
        };
        Ok(Self {
            phase,
            budget: Budget::new(tasks, reserved, children)?,
            actor,
            parent,
            uid,
            reaper_uid,
            threads,
            held,
            reaped,
            replacement,
            descriptors,
            cause,
            errno: i32::from_be_bytes(errno.to_be_bytes()),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn budget_requires_finite_children_inside_the_declared_total() {
        assert_eq!(Budget::new(6, 4, 2).unwrap().tasks, 6);
        for values in [(0, 0, 0), (4, 4, 0), (6, 3, 2), (u32::MAX, u32::MAX, 1)] {
            assert_eq!(Budget::new(values.0, values.1, values.2), Err(Invalid::Budget));
        }
    }
    #[test]
    fn checked_record_rejects_unknown_or_truncated_messages() {
        let report = Report {
            phase: Phase::Refused,
            budget: Budget::new(6, 4, 2).unwrap(),
            actor: 2,
            parent: 1,
            uid: 200_000,
            reaper_uid: 0,
            threads: 1,
            held: 2,
            reaped: 0,
            replacement: 0,
            descriptors: 5,
            cause: Cause::ForkRefused,
            errno: 11,
        };
        let bytes = report.encode();
        assert_eq!(Report::decode(&bytes), Ok(report));
        for end in 0..bytes.len() {
            assert_eq!(Report::decode(&bytes[..end]), Err(Invalid::Length));
        }
        let mut bad = bytes;
        bad[7] = 255;
        assert_eq!(Report::decode(&bad), Err(Invalid::Phase));
        let mut bad = bytes;
        bad[3] = 2;
        assert_eq!(Report::decode(&bad), Err(Invalid::Version));
        let mut bad = bytes;
        bad[59] = 255;
        assert_eq!(Report::decode(&bad), Err(Invalid::Cause));
        assert_eq!(Command::decode([0]), Err(Invalid::Command));
    }
}
