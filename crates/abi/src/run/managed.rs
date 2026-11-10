//! Canonical managed output, authenticated by its owning run connection.

use core::fmt;

/// Four tag bytes, a member's u32 index and its channel's u64 byte offset.
pub const HEADER: usize = 4 + size_of::<u32>() + size_of::<u64>();
pub const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Channel {
    Stdout = super::kind::STDOUT,
    Stderr = super::kind::STDERR,
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
        let event = match self.event {
            Event::Open(label) => {
                payload.copy_from_slice(label.as_bytes());
                0
            }
            Event::Data(bytes) => {
                payload.copy_from_slice(bytes);
                1
            }
            Event::End => 2,
            Event::Aborted(errno) => {
                payload.copy_from_slice(&errno.to_be_bytes());
                3
            }
        };
        let (tags, rest) = header.split_first_chunk_mut::<4>().ok_or(Error::Header)?;
        *tags = [VERSION, event, self.channel as u8, 0];
        let (member, offset) = rest
            .split_first_chunk_mut::<{ size_of::<u32>() }>()
            .ok_or(Error::Header)?;
        *member = self.member.to_be_bytes();
        offset.copy_from_slice(&self.offset.to_be_bytes());
        Ok(len)
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
