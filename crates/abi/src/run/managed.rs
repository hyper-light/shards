//! Canonical managed output, authenticated by its owning run connection.

use alloc::vec::Vec;
use core::fmt;

/// Four tag bytes, a member's u32 index and its channel's u64 byte offset.
pub const HEADER: usize = 4 + size_of::<u32>() + size_of::<u64>();
pub const VERSION: u8 = 1;
pub const CAPACITY_KEY: &[u8] = b"managed-capacity=";
pub const TOKEN_KEY: &[u8] = b"managed-token=";
pub const MEMBERS_KEY: &[u8] = b"managed-members=";
pub const ACTIVE_KEY: &[u8] = b"managed-active=";

pub fn active(setup: &[Vec<u8>]) -> Result<Option<&[[u8; size_of::<u32>()]]>, Error> {
    let mut entries = setup.iter().filter_map(|e| e.strip_prefix(ACTIVE_KEY));
    let Some(bytes) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() {
        return Err(Error::Member);
    }
    let declared = members(setup)?.ok_or(Error::Member)?;
    let (ids, tail) = bytes.as_chunks::<{ size_of::<u32>() }>();
    if ids.is_empty() || !tail.is_empty() {
        return Err(Error::Member);
    }
    let mut previous = None;
    for id in ids {
        let id = u32::from_be_bytes(*id);
        if id >= declared || previous.is_some_and(|p| p >= id) {
            return Err(Error::Member);
        }
        previous = Some(id);
    }
    Ok(Some(ids))
}
/// The members whose stdin is a stream their owner writes: each a u32, big endian,
/// ascending, each an active member's ([`ACTIVE_KEY`]). Every other member's stdin is
/// `/dev/null`: input is closed unless its owner asks for it as the run is made (D231
/// decision §3.3, managed-runtime-plan.md P3's input policy).
pub const INPUT_KEY: &[u8] = b"managed-input=";
/// One input member's connection token: its index (a u32, big endian), then the
/// [`TOKEN`](super::TOKEN) bytes the VM process minted for it. The member's input arrives on
/// a connection of its own, which init dials to [`EXEC_PORT`](super::EXEC_PORT) saying this
/// token, so that vsock's own credit backs it up and an unread stdin holds up nothing else.
pub const INPUT_TOKEN_KEY: &[u8] = b"managed-input-token=";
/// [`InputEnded`]'s payload: a member's u32, the bytes its stdin took (u64) and its errno
/// (i32, 0 for its owner's end).
pub const INPUT_ENDED_LEN: usize = size_of::<u32>() + size_of::<u64>() + size_of::<i32>();

/// The members [`INPUT_KEY`] names: none where it is absent; refused where it repeats, is
/// empty, is not in ascending order, or names a member that is not active.
pub fn inputs(setup: &[Vec<u8>]) -> Result<Option<&[[u8; size_of::<u32>()]]>, Error> {
    let mut entries = setup.iter().filter_map(|e| e.strip_prefix(INPUT_KEY));
    let Some(bytes) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() {
        return Err(Error::Member);
    }
    let active = active(setup)?.ok_or(Error::Member)?;
    let (ids, tail) = bytes.as_chunks::<{ size_of::<u32>() }>();
    if ids.is_empty() || !tail.is_empty() {
        return Err(Error::Member);
    }
    let mut previous = None;
    for id in ids {
        let value = u32::from_be_bytes(*id);
        if previous.is_some_and(|p| p >= value) || !active.contains(id) {
            return Err(Error::Member);
        }
        previous = Some(value);
    }
    Ok(Some(ids))
}

/// One [`INPUT_TOKEN_KEY`] entry: the member and its token.
pub fn input_token(entry: &[u8]) -> Result<Option<(u32, [u8; super::TOKEN])>, Error> {
    let Some(bytes) = entry.strip_prefix(INPUT_TOKEN_KEY) else {
        return Ok(None);
    };
    let (member, token) = bytes
        .split_first_chunk::<{ size_of::<u32>() }>()
        .ok_or(Error::Payload)?;
    let token = <[u8; super::TOKEN]>::try_from(token).map_err(|_| Error::Payload)?;
    Ok(Some((u32::from_be_bytes(*member), token)))
}

/// Each input member's token, in the order [`inputs`] names them: refused unless there is
/// exactly one for each, and none for a member it does not name.
pub fn input_tokens(setup: &[Vec<u8>]) -> Result<Vec<(u32, [u8; super::TOKEN])>, Error> {
    let members = inputs(setup)?.unwrap_or_default();
    let mut tokens = Vec::new();
    tokens
        .try_reserve_exact(members.len())
        .map_err(|_| Error::Capacity)?;
    for entry in setup {
        if let Some(token) = input_token(entry)? {
            if tokens.len() == members.len() {
                return Err(Error::Member);
            }
            tokens.push(token);
        }
    }
    tokens.sort_unstable_by_key(|(member, _)| *member);
    if tokens.len() != members.len()
        || tokens
            .iter()
            .zip(members)
            .any(|((got, _), want)| *got != u32::from_be_bytes(*want))
    {
        return Err(Error::Member);
    }
    Ok(tokens)
}

/// How a member's input ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEnd {
    /// Its owner's end came, every byte before it written to the member's stdin, which
    /// init then closed.
    Closed,
    /// It ended first, for this errno: `EPIPE`, the member's stdin no longer read; the
    /// input connection's own error, or `ECONNRESET` where it ended without its owner's
    /// end; `EPROTO`, a frame not of the input's protocol; `ENOMEM`, no room for its relay.
    Aborted(i32),
}

/// Init's word, on the managed control connection, that a member's input has ended, and
/// how many bytes its stdin took: told once a member, before the control scope ends. The
/// owner's end and every failure of the input connection are told alike, never dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputEnded {
    pub member: u32,
    pub written: u64,
    pub end: InputEnd,
}

impl InputEnded {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (member, rest) = bytes
            .split_first_chunk::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        let (written, errno) = rest
            .split_first_chunk::<{ size_of::<u64>() }>()
            .ok_or(Error::Header)?;
        let errno = i32::from_be_bytes(errno.try_into().map_err(|_| Error::Payload)?);
        let end = match errno {
            0 => InputEnd::Closed,
            1.. => InputEnd::Aborted(errno),
            _ => return Err(Error::Payload),
        };
        Ok(Self {
            member: u32::from_be_bytes(*member),
            written: u64::from_be_bytes(*written),
            end,
        })
    }

    pub fn encode(self) -> Result<[u8; INPUT_ENDED_LEN], Error> {
        let errno = match self.end {
            InputEnd::Closed => 0,
            InputEnd::Aborted(errno) if errno > 0 => errno,
            InputEnd::Aborted(_) => return Err(Error::Payload),
        };
        let mut bytes = [0; INPUT_ENDED_LEN];
        let (member, rest) = bytes
            .split_first_chunk_mut::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        *member = self.member.to_be_bytes();
        let (written, tail) = rest
            .split_first_chunk_mut::<{ size_of::<u64>() }>()
            .ok_or(Error::Header)?;
        *written = self.written.to_be_bytes();
        tail.copy_from_slice(&errno.to_be_bytes());
        Ok(bytes)
    }
}

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

impl core::error::Error for Error {}

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
        let active = [ACTIVE_KEY, &0u32.to_be_bytes(), &2u32.to_be_bytes()].concat();
        let setup = [b"managed-members=3".to_vec(), active];
        assert_eq!(
            super::active(&setup).unwrap().unwrap(),
            &[0u32.to_be_bytes(), 2u32.to_be_bytes()]
        );
        let invalid = [ACTIVE_KEY, &2u32.to_be_bytes(), &2u32.to_be_bytes()].concat();
        assert!(super::active(&[b"managed-members=3".to_vec(), invalid]).is_err());
    }

    fn input_setup(inputs: &[u32], tokens: &[(u32, u8)]) -> Vec<Vec<u8>> {
        let active = [
            ACTIVE_KEY,
            &0u32.to_be_bytes(),
            &2u32.to_be_bytes(),
            &5u32.to_be_bytes(),
        ]
        .concat();
        let mut setup = alloc::vec![b"managed-members=6".to_vec(), active];
        if !inputs.is_empty() {
            let named: Vec<u8> = inputs.iter().flat_map(|m| m.to_be_bytes()).collect();
            setup.push([INPUT_KEY, &named].concat());
        }
        for (member, fill) in tokens {
            setup.push(
                [
                    INPUT_TOKEN_KEY,
                    &member.to_be_bytes(),
                    &[*fill; super::super::TOKEN],
                ]
                .concat(),
            );
        }
        setup
    }

    #[test]
    fn input_members_are_active_ascending_and_each_has_one_token() {
        let setup = input_setup(&[2, 5], &[(5, 0xb), (2, 0xa)]);
        assert_eq!(
            inputs(&setup).unwrap().unwrap(),
            &[2u32.to_be_bytes(), 5u32.to_be_bytes()]
        );
        let tokens = input_tokens(&setup).unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], (2, [0xa; super::super::TOKEN]));
        assert_eq!(tokens[1], (5, [0xb; super::super::TOKEN]));
        // No entry: no input, and no token may come without one.
        assert_eq!(inputs(&input_setup(&[], &[])), Ok(None));
        assert!(input_tokens(&input_setup(&[], &[])).unwrap().is_empty());
        assert_eq!(input_tokens(&input_setup(&[], &[(2, 1)])), Err(Error::Member));
        // A member not active, out of order, repeated, or the entry given twice.
        for named in [&[1u32][..], &[5, 2], &[2, 2]] {
            assert_eq!(inputs(&input_setup(named, &[])), Err(Error::Member), "{named:?}");
        }
        let mut twice = input_setup(&[2], &[(2, 1)]);
        twice.push([INPUT_KEY, &2u32.to_be_bytes()].concat());
        assert_eq!(inputs(&twice), Err(Error::Member));
        // A token missing, one too many, one for another member, one cut short.
        assert_eq!(input_tokens(&input_setup(&[2, 5], &[(2, 1)])), Err(Error::Member));
        assert_eq!(
            input_tokens(&input_setup(&[2], &[(2, 1), (2, 2)])),
            Err(Error::Member)
        );
        assert_eq!(input_tokens(&input_setup(&[2], &[(5, 1)])), Err(Error::Member));
        let mut short = input_setup(&[2], &[]);
        short.push([INPUT_TOKEN_KEY, &2u32.to_be_bytes(), &[1; 3]].concat());
        assert_eq!(input_tokens(&short), Err(Error::Payload));
        // An input without managed activation names no member.
        let bare = [[INPUT_KEY, &0u32.to_be_bytes()].concat()];
        assert_eq!(inputs(&bare), Err(Error::Member));
    }

    #[test]
    fn an_inputs_end_round_trips_and_refuses_a_negative_errno() {
        for end in [InputEnd::Closed, InputEnd::Aborted(32), InputEnd::Aborted(104)] {
            let said = InputEnded {
                member: u32::MAX,
                written: u64::MAX,
                end,
            };
            let bytes = said.encode().unwrap();
            assert_eq!(bytes.len(), INPUT_ENDED_LEN);
            assert_eq!(InputEnded::decode(&bytes), Ok(said));
            assert!(InputEnded::decode(&bytes[..INPUT_ENDED_LEN - 1]).is_err());
        }
        let negative = InputEnded {
            member: 0,
            written: 0,
            end: InputEnd::Aborted(-1),
        };
        assert_eq!(negative.encode(), Err(Error::Payload));
        let mut bytes = InputEnded {
            end: InputEnd::Closed,
            ..negative
        }
        .encode()
        .unwrap();
        bytes[INPUT_ENDED_LEN - 4..].copy_from_slice(&(-5i32).to_be_bytes());
        assert_eq!(InputEnded::decode(&bytes), Err(Error::Payload));
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
