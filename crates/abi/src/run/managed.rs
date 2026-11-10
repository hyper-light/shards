//! Canonical managed output, authenticated by its owning run connection.

use alloc::vec::Vec;
use core::fmt;

/// Four tag bytes, a member's u32 index and its channel's u64 byte offset.
pub const HEADER: usize = 4 + size_of::<u32>() + size_of::<u64>();
pub const VERSION: u8 = 1;
pub const CAPACITY_KEY: &[u8] = b"managed-capacity=";
pub const TOKEN_KEY: &[u8] = b"managed-token=";
pub const MEMBERS_KEY: &[u8] = b"managed-members=";
pub const CREDIT_LEN: usize = size_of::<u32>() + size_of::<u8>() + size_of::<u64>();
pub const ABORT_LEN: usize = size_of::<u32>() + size_of::<i32>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Abort {
    pub member: u32,
    pub errno: i32,
}

impl Abort {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (member, rest) = bytes
            .split_first_chunk::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        let errno = i32::from_be_bytes(rest.try_into().map_err(|_| Error::Payload)?);
        if errno <= 0 {
            return Err(Error::Payload);
        }
        Ok(Self {
            member: u32::from_be_bytes(*member),
            errno,
        })
    }
    pub fn encode(self) -> Result<[u8; ABORT_LEN], Error> {
        let mut bytes = [0; ABORT_LEN];
        let (member, rest) = bytes
            .split_first_chunk_mut::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        *member = self.member.to_be_bytes();
        rest.copy_from_slice(&self.errno.to_be_bytes());
        Self::decode(&bytes)?;
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credit {
    pub member: u32,
    pub channel: Channel,
    pub capacity: u64,
}

impl Credit {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (member, rest) = bytes
            .split_first_chunk::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        let (channel, rest) = rest.split_first().ok_or(Error::Payload)?;
        let channel = Channel::try_from(*channel)?;
        let capacity = u64::from_be_bytes(rest.try_into().map_err(|_| Error::Payload)?);
        if capacity < (HEADER + size_of::<i32>()) as u64 || capacity > u64::from(super::MAX_PAYLOAD) {
            return Err(Error::Capacity);
        }
        Ok(Self {
            member: u32::from_be_bytes(*member),
            channel,
            capacity,
        })
    }

    pub fn encode(self) -> Result<[u8; CREDIT_LEN], Error> {
        let mut bytes = [0; CREDIT_LEN];
        let (member, rest) = bytes
            .split_first_chunk_mut::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        *member = self.member.to_be_bytes();
        let (channel, rest) = rest.split_first_mut().ok_or(Error::Payload)?;
        *channel = self.channel as u8;
        rest.copy_from_slice(&self.capacity.to_be_bytes());
        Self::decode(&bytes)?;
        Ok(bytes)
    }
}

/// Reads the numeric route before allocating a record; full Record::decode is still required.
pub fn route_prefix(bytes: &[u8]) -> Result<(u32, Channel), Error> {
    let (tags, rest) = bytes.split_first_chunk::<4>().ok_or(Error::Header)?;
    let [_, _, channel, _] = *tags;
    let (member, _) = rest
        .split_first_chunk::<{ size_of::<u32>() }>()
        .ok_or(Error::Header)?;
    Ok((u32::from_be_bytes(*member), Channel::try_from(channel)?))
}

pub fn members(setup: &[Vec<u8>]) -> Result<Option<u32>, Error> {
    let mut entries = setup.iter().filter_map(|e| e.strip_prefix(MEMBERS_KEY));
    let Some(value) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() {
        return Err(Error::Member);
    }
    let value = core::str::from_utf8(value)
        .map_err(|_| Error::Member)?
        .parse::<u32>()
        .map_err(|_| Error::Member)?;
    if value == 0 {
        return Err(Error::Member);
    }
    Ok(Some(value))
}

pub fn capacity(setup: &[Vec<u8>]) -> Result<Option<usize>, Error> {
    let mut entries = setup.iter().filter_map(|e| e.strip_prefix(CAPACITY_KEY));
    let Some(value) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() {
        return Err(Error::Capacity);
    }
    let value = core::str::from_utf8(value)
        .map_err(|_| Error::Capacity)?
        .parse::<usize>()
        .map_err(|_| Error::Capacity)?;
    if value < HEADER + size_of::<i32>() || value > super::MAX_PAYLOAD as usize {
        return Err(Error::Capacity);
    }
    Ok(Some(value))
}

pub fn token(setup: &[Vec<u8>]) -> Result<Option<[u8; super::TOKEN]>, Error> {
    let mut entries = setup.iter().filter_map(|e| e.strip_prefix(TOKEN_KEY));
    let Some(value) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() {
        return Err(Error::Payload);
    }
    Ok(Some(value.try_into().map_err(|_| Error::Payload)?))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Channel {
    Stdout = super::kind::STDOUT,
    Stderr = super::kind::STDERR,
}

impl TryFrom<u8> for Channel {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            super::kind::STDOUT => Ok(Self::Stdout),
            super::kind::STDERR => Ok(Self::Stderr),
            _ => Err(Error::Channel),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event<'a> {
    Open(&'a str),
    Data(&'a [u8]),
    End,
    Aborted(i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record<'a> {
    pub member: u32,
    pub channel: Channel,
    pub offset: u64,
    pub event: Event<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Header,
    Version,
    Reserved,
    Channel,
    Event,
    Payload,
    Offset,
    State,
    Capacity,
    Member,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Header => "incomplete managed output header",
            Self::Version => "unsupported managed output version",
            Self::Reserved => "nonzero managed output reserved field",
            Self::Channel => "unknown managed output channel",
            Self::Event => "unknown managed output event",
            Self::Payload => "invalid managed output payload",
            Self::Offset => "invalid managed output offset",
            Self::State => "invalid managed output transition",
            Self::Capacity => "managed output exceeds admitted capacity",
            Self::Member => "managed output member was not admitted",
        })
    }
}

impl Record<'_> {
    pub fn decode(bytes: &[u8]) -> Result<Record<'_>, Error> {
        let (header, payload) = bytes.split_first_chunk::<HEADER>().ok_or(Error::Header)?;
        let (tags, rest) = header.split_first_chunk::<4>().ok_or(Error::Header)?;
        let [version, event, channel, reserved] = *tags;
        if version != VERSION {
            return Err(Error::Version);
        }
        if reserved != 0 {
            return Err(Error::Reserved);
        }
        let channel = match channel {
            super::kind::STDOUT => Channel::Stdout,
            super::kind::STDERR => Channel::Stderr,
            _ => return Err(Error::Channel),
        };
        let (member, offset) = rest
            .split_first_chunk::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        let member = u32::from_be_bytes(*member);
        let offset = u64::from_be_bytes(offset.try_into().map_err(|_| Error::Header)?);
        let event = match event {
            0 => Event::Open(core::str::from_utf8(payload).map_err(|_| Error::Payload)?),
            1 => Event::Data(payload),
            2 if payload.is_empty() => Event::End,
            3 => Event::Aborted(i32::from_be_bytes(
                payload.try_into().map_err(|_| Error::Payload)?,
            )),
            2 => return Err(Error::Payload),
            _ => return Err(Error::Event),
        };
        let record = Record {
            member,
            channel,
            offset,
            event,
        };
        record.encoded_len()?;
        Ok(record)
    }

    pub fn encoded_len(self) -> Result<usize, Error> {
        let len = match self.event {
            Event::Open(label) if !label.is_empty() && self.offset == 0 => label.len(),
            Event::Data(bytes) if !bytes.is_empty() => {
                self.offset
                    .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::Offset)?)
                    .ok_or(Error::Offset)?;
                bytes.len()
            }
            Event::End => 0,
            Event::Aborted(errno) if errno > 0 => size_of::<i32>(),
            _ => return Err(Error::Payload),
        };
        HEADER.checked_add(len).ok_or(Error::Capacity)
    }

    pub fn encode(self, into: &mut [u8]) -> Result<usize, Error> {
        let len = self.encoded_len()?;
        let into = into.get_mut(..len).ok_or(Error::Capacity)?;
        let (header, payload) = into.split_first_chunk_mut::<HEADER>().ok_or(Error::Header)?;
        match self.event {
            Event::Open(label) => {
                payload.copy_from_slice(label.as_bytes());
            }
            Event::Data(bytes) => {
                payload.copy_from_slice(bytes);
            }
            Event::End => {}
            Event::Aborted(errno) => {
                payload.copy_from_slice(&errno.to_be_bytes());
            }
        }
        *header = self.header()?;
        Ok(len)
    }

    pub fn header(self) -> Result<[u8; HEADER], Error> {
        self.encoded_len()?;
        let mut header = [0; HEADER];
        let event = match self.event {
            Event::Open(_) => 0,
            Event::Data(_) => 1,
            Event::End => 2,
            Event::Aborted(_) => 3,
        };
        let (tags, rest) = header.split_first_chunk_mut::<4>().ok_or(Error::Header)?;
        *tags = [VERSION, event, self.channel as u8, 0];
        let (member, offset) = rest
            .split_first_chunk_mut::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        *member = self.member.to_be_bytes();
        offset.copy_from_slice(&self.offset.to_be_bytes());
        Ok(header)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StreamState {
    #[default]
    New,
    Open {
        offset: u64,
    },
    Ended {
        offset: u64,
    },
    Aborted {
        offset: u64,
        errno: i32,
    },
}

impl StreamState {
    pub fn accept(&mut self, record: Record<'_>) -> Result<(), Error> {
        record.encoded_len()?;
        let next = match (*self, record.event) {
            (Self::New, Event::Open(_)) => Self::Open { offset: 0 },
            (Self::Open { offset }, event) if offset == record.offset => match event {
                Event::Data(bytes) => Self::Open {
                    offset: offset
                        .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::Offset)?)
                        .ok_or(Error::Offset)?,
                },
                Event::End => Self::Ended { offset },
                Event::Aborted(errno) => Self::Aborted { offset, errno },
                Event::Open(_) => return Err(Error::State),
            },
            (Self::Open { .. }, _) => return Err(Error::Offset),
            _ => return Err(Error::State),
        };
        *self = next;
        Ok(())
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Ended { .. } | Self::Aborted { .. })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn credits_and_aborts_have_checked_scoped_routes() {
        for channel in [Channel::Stdout, Channel::Stderr] {
            let credit = Credit {
                member: u32::MAX,
                channel,
                capacity: (HEADER + size_of::<i32>()) as u64,
            };
            let bytes = credit.encode().unwrap();
            assert_eq!(bytes.len(), 13);
            assert_eq!(Credit::decode(&bytes).unwrap(), credit);
            assert!(Credit::decode(&bytes[..bytes.len() - 1]).is_err());
            let mut bad = bytes;
            bad[4] = 0;
            assert_eq!(Credit::decode(&bad), Err(Error::Channel));
            assert!(
                Credit {
                    capacity: 0,
                    ..credit
                }
                .encode()
                .is_err()
            );
        }
        let abort = Abort { member: 9, errno: 5 };
        assert_eq!(Abort::decode(&abort.encode().unwrap()).unwrap(), abort);
        assert!(Abort { errno: 0, ..abort }.encode().is_err());
    }

    #[test]
    fn admission_is_unique_finite_and_checked() {
        let mut setup = alloc::vec![b"managed-capacity=20".to_vec(), b"managed-members=2".to_vec()];
        assert_eq!(capacity(&setup), Ok(Some(20)));
        assert_eq!(members(&setup), Ok(Some(2)));
        setup.push(b"managed-capacity=20".to_vec());
        assert_eq!(capacity(&setup), Err(Error::Capacity));
        for value in [b"0".as_slice(), b"999999999999999999999999999999999999999", b"x"] {
            let entry = [CAPACITY_KEY, value].concat();
            assert!(capacity(&[entry]).is_err());
        }
        assert!(members(&[b"managed-members=0".to_vec()]).is_err());
        assert!(token(&[b"managed-token=short".to_vec()]).is_err());
    }

    #[test]
    fn canonical_records_round_trip_binary_channels_and_terminals() {
        let payload = b"\0\xff\x80\x1b[2Jno-newline";
        let end = payload.len() as u64;
        for channel in [Channel::Stdout, Channel::Stderr] {
            let mut state = StreamState::New;
            for (offset, event) in [
                (0, Event::Open("harness drive")),
                (0, Event::Data(payload)),
                (end, Event::End),
            ] {
                let record = Record {
                    member: u32::MAX,
                    channel,
                    offset,
                    event,
                };
                let mut bytes = [0; 128];
                let n = record.encode(&mut bytes).unwrap();
                let decoded = Record::decode(&bytes[..n]).unwrap();
                assert_eq!(decoded, record);
                state.accept(decoded).unwrap();
            }
            assert_eq!(state, StreamState::Ended { offset: end });
        }
        let record = Record {
            member: 0,
            channel: Channel::Stderr,
            offset: 1,
            event: Event::Aborted(5),
        };
        let mut bytes = [0; HEADER + size_of::<i32>()];
        record.encode(&mut bytes).unwrap();
        assert_eq!(Record::decode(&bytes).unwrap(), record);
    }

    #[test]
    fn malformed_tags_lengths_offsets_and_transitions_are_refused() {
        let open = Record {
            member: 0,
            channel: Channel::Stdout,
            offset: 0,
            event: Event::Open("agent a"),
        };
        let mut bytes = [0; HEADER + 7];
        open.encode(&mut bytes).unwrap();
        for n in 0..HEADER {
            assert_eq!(Record::decode(&bytes[..n]), Err(Error::Header));
        }
        for (field, value, error) in [
            (0, 0, Error::Version),
            (1, 4, Error::Event),
            (2, 0, Error::Channel),
            (3, 1, Error::Reserved),
        ] {
            let mut bad = bytes;
            bad[field] = value;
            assert_eq!(Record::decode(&bad), Err(error));
        }
        let mut state = StreamState::New;
        let data = Record {
            event: Event::Data(b"x"),
            ..open
        };
        assert_eq!(state.accept(data), Err(Error::State));
        state.accept(open).unwrap();
        assert_eq!(state.accept(open), Err(Error::State));
        assert_eq!(state.accept(Record { offset: 1, ..data }), Err(Error::Offset));
        state.accept(data).unwrap();
        state
            .accept(Record {
                offset: 1,
                event: Event::Aborted(5),
                ..open
            })
            .unwrap();
        assert!(state.terminal());
        assert_eq!(state.accept(data), Err(Error::State));
        assert!(
            Record {
                offset: u64::MAX,
                ..data
            }
            .encoded_len()
            .is_err()
        );
        assert!(
            Record {
                event: Event::Data(b""),
                ..open
            }
            .encoded_len()
            .is_err()
        );
        assert!(
            Record {
                event: Event::Aborted(0),
                ..open
            }
            .encoded_len()
            .is_err()
        );
        assert!(
            Record {
                event: Event::Open(""),
                ..open
            }
            .encoded_len()
            .is_err()
        );
        assert!(open.encode(&mut [0; HEADER]).is_err());
    }
}
