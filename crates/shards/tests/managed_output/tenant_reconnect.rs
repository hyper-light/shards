//! One observed unacknowledged Data record, retained as a reconnect fixture witness.
use shards_abi::run::managed::{Channel, Error, Event, Record, StreamState};

#[path = "tenant_observer.rs"]
mod fixture;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReplayError {
    WireOffset { expected: u64, actual: u64 },
    WireLength,
    Canonical(Error),
    Identity { member: u32, channel: Channel },
    NotData,
    ChangedBytes,
}

pub(super) struct RetainedRecord {
    start: u64,
    bytes: Vec<u8>,
    before: StreamState,
}

impl RetainedRecord {
    pub(super) fn hold(
        member: u32,
        channel: Channel,
        start: u64,
        bytes: Vec<u8>,
        before: StreamState,
    ) -> Result<Self, ReplayError> {
        let record = Record::decode(&bytes).map_err(ReplayError::Canonical)?;
        if record.member != member || record.channel != channel {
            return Err(ReplayError::Identity {
                member: record.member,
                channel: record.channel,
            });
        }
        if !matches!(record.event, Event::Data(_)) {
            return Err(ReplayError::NotData);
        }
        let mut after = before;
        after.accept(record).map_err(ReplayError::Canonical)?;
        start
            .checked_add(u64::try_from(bytes.len()).map_err(|_| ReplayError::WireLength)?)
            .ok_or(ReplayError::WireLength)?;
        Ok(Self { start, bytes, before })
    }

    pub(super) fn replay(&self, offset: u64, bytes: &[u8]) -> Result<(u64, StreamState), ReplayError> {
        if offset != self.start {
            return Err(ReplayError::WireOffset {
                expected: self.start,
                actual: offset,
            });
        }
        if bytes != self.bytes {
            return Err(ReplayError::ChangedBytes);
        }
        let mut after = self.before;
        after
            .accept(Record::decode(bytes).map_err(ReplayError::Canonical)?)
            .map_err(ReplayError::Canonical)?;
        let end = self
            .start
            .checked_add(u64::try_from(bytes.len()).map_err(|_| ReplayError::WireLength)?)
            .ok_or(ReplayError::WireLength)?;
        Ok((end, after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(record: Record<'_>) -> Vec<u8> {
        let mut bytes = vec![0; record.encoded_len().unwrap()];
        assert_eq!(record.encode(&mut bytes).unwrap(), bytes.len());
        bytes
    }

    fn opened() -> (u64, StreamState) {
        let mut state = StreamState::New;
        let open = Record {
            member: 2,
            channel: Channel::Stdout,
            offset: 0,
            event: Event::Open("harness driver"),
        };
        state.accept(open).unwrap();
        let prefix = Record {
            member: 2,
            channel: Channel::Stdout,
            offset: 0,
            event: Event::Data(b"prefix\0"),
        };
        state.accept(prefix).unwrap();
        (
            u64::try_from(open.encoded_len().unwrap() + prefix.encoded_len().unwrap()).unwrap(),
            state,
        )
    }

    fn tail() -> Record<'static> {
        Record {
            member: 2,
            channel: Channel::Stdout,
            offset: u64::try_from(b"prefix\0".len()).unwrap(),
            event: Event::Data(b"tail\xff"),
        }
    }

    #[test]
    fn nonzero_retained_wire_cursor_advances_payload_once_then_accepts_new_production() {
        let (start, before) = opened();
        let bytes = encoded(tail());
        let held = RetainedRecord::hold(2, Channel::Stdout, start, bytes.clone(), before).unwrap();
        let (end, mut after) = held.replay(start, &bytes).unwrap();
        assert_ne!(start, 0);
        assert_eq!(end, start + u64::try_from(bytes.len()).unwrap());
        let payload_end = u64::try_from(b"prefix\0tail\xff".len()).unwrap();
        assert_eq!(after, StreamState::Open { offset: payload_end });
        let later = Record {
            member: 2,
            channel: Channel::Stdout,
            offset: payload_end,
            event: Event::Data(b"after-owner-release\0\x80"),
        };
        after.accept(later).unwrap();
        assert_eq!(
            after,
            StreamState::Open {
                offset: payload_end + u64::try_from(b"after-owner-release\0\x80".len()).unwrap()
            }
        );
    }

    #[test]
    fn replay_rejects_zero_forward_shifted_cursors_and_same_length_changed_bytes() {
        let (start, before) = opened();
        let bytes = encoded(tail());
        let held = RetainedRecord::hold(2, Channel::Stdout, start, bytes.clone(), before).unwrap();
        for actual in [0, start + 1, start + u64::try_from(bytes.len()).unwrap()] {
            assert_eq!(
                held.replay(actual, &bytes),
                Err(ReplayError::WireOffset {
                    expected: start,
                    actual
                })
            );
        }
        let mut changed = bytes.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert_eq!(held.replay(start, &changed), Err(ReplayError::ChangedBytes));
        let mut truncated = bytes;
        truncated.pop();
        assert_eq!(held.replay(start, &truncated), Err(ReplayError::ChangedBytes));
    }

    #[test]
    fn held_witness_requires_scoped_data_at_the_prior_canonical_payload_tail() {
        let (start, before) = opened();
        for (member, channel) in [(1, Channel::Stdout), (2, Channel::Stderr)] {
            let bytes = encoded(Record {
                member,
                channel,
                ..tail()
            });
            assert!(matches!(
                RetainedRecord::hold(2, Channel::Stdout, start, bytes, before),
                Err(ReplayError::Identity { .. })
            ));
        }
        let bytes = encoded(Record {
            event: Event::End,
            ..tail()
        });
        assert!(matches!(
            RetainedRecord::hold(2, Channel::Stdout, start, bytes, before),
            Err(ReplayError::NotData)
        ));
        let bytes = encoded(Record {
            offset: tail().offset + 1,
            ..tail()
        });
        assert!(matches!(
            RetainedRecord::hold(2, Channel::Stdout, start, bytes, before),
            Err(ReplayError::Canonical(Error::Offset))
        ));
        let bytes = encoded(tail());
        assert!(matches!(
            RetainedRecord::hold(2, Channel::Stdout, u64::MAX, bytes, before),
            Err(ReplayError::WireLength)
        ));
    }
}
