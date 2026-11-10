//! Pure input/cursor contracts; no endpoint, transport model or lifecycle owner.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AckCase {
    Partial,
    Replay,
    Forward,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Invalid {
    Case,
    ZeroBound,
    Overflow,
    OuterBound,
    RecordWindow,
    NoPriorAck,
}

impl AckCase {
    pub(crate) fn parse(value: &str) -> Result<Self, Invalid> {
        match value {
            "ack-partial" => Ok(Self::Partial),
            "ack-replay" => Ok(Self::Replay),
            "ack-forward" => Ok(Self::Forward),
            _ => Err(Invalid::Case),
        }
    }

    pub(crate) fn cursor(self, start: u64, end: u64, last_valid_ack: u64) -> Result<u64, Invalid> {
        if start != last_valid_ack || end <= start {
            return Err(Invalid::RecordWindow);
        }
        match self {
            Self::Partial => start
                .checked_add(1)
                .filter(|at| *at < end)
                .ok_or(Invalid::RecordWindow),
            Self::Replay if last_valid_ack != 0 => Ok(last_valid_ack),
            Self::Replay => Err(Invalid::NoPriorAck),
            Self::Forward => end.checked_add(1).ok_or(Invalid::Overflow),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalCase {
    Duplicate,
    Forward,
    Finish,
    Resubscribe,
}

impl TerminalCase {
    pub(crate) fn cursor(self, end: u64) -> Result<u64, Invalid> {
        match self {
            Self::Duplicate | Self::Finish | Self::Resubscribe => Ok(end),
            Self::Forward => end.checked_add(1).ok_or(Invalid::Overflow),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlCase {
    UnknownTag,
    WrongKnownTag,
    Overdeclared,
    AckLength,
    ShortHeader,
    HeaderOnly,
    ShortAck,
}

impl ControlCase {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "control-unknown-tag" => Self::UnknownTag,
            "control-wrong-tag" => Self::WrongKnownTag,
            "control-overdeclared" => Self::Overdeclared,
            "control-ack-length" => Self::AckLength,
            "control-short-header" => Self::ShortHeader,
            "control-header-only" => Self::HeaderOnly,
            "control-short-ack" => Self::ShortAck,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    Ack(AckCase),
    Control(ControlCase),
    Terminal(TerminalCase),
}

impl Selection {
    pub(crate) fn parse(value: &str) -> Result<Self, Invalid> {
        match value {
            "terminal-end-duplicate" => Ok(Self::Terminal(TerminalCase::Duplicate)),
            "terminal-end-forward" => Ok(Self::Terminal(TerminalCase::Forward)),
            "terminal-end-finish" => Ok(Self::Terminal(TerminalCase::Finish)),
            "terminal-end-resubscribe" => Ok(Self::Terminal(TerminalCase::Resubscribe)),
            _ => ControlCase::parse(value)
                .map(Self::Control)
                .map(Ok)
                .unwrap_or_else(|| AckCase::parse(value).map(Self::Ack)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub phase_ms: u64,
    pub work_ms: u64,
    pub cleanup_ms: u64,
    pub outer_ms: u64,
}

pub(crate) struct Deadlines {
    pub work: Instant,
    pub cleanup: Instant,
    pub outer: Instant,
}

impl Limits {
    pub(crate) fn deadlines(self, began: Instant) -> Result<Deadlines, Invalid> {
        if [self.phase_ms, self.work_ms, self.cleanup_ms, self.outer_ms].contains(&0) {
            return Err(Invalid::ZeroBound);
        }
        let total = self
            .phase_ms
            .checked_add(self.cleanup_ms)
            .ok_or(Invalid::Overflow)?;
        if total > self.outer_ms {
            return Err(Invalid::OuterBound);
        }
        let at = |ms| {
            began
                .checked_add(Duration::from_millis(ms))
                .ok_or(Invalid::Overflow)
        };
        at(self.work_ms)?;
        Ok(Deadlines {
            work: at(self.phase_ms)?,
            cleanup: at(total)?,
            outer: at(self.outer_ms)?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use shards_abi::run::managed::{Channel, Event, Record, StreamState};

    #[test]
    fn application_ack_uses_encoded_record_end_not_canonical_payload_offset() {
        let open = Record {
            member: 1,
            channel: Channel::Stdout,
            offset: 0,
            event: Event::Open("helper"),
        };
        let data = Record {
            event: Event::Data(b"\0\xffowned-data"),
            ..open
        };
        let mut state = StreamState::New;
        let mut wire = 0u64;
        let mut prior_ack = 0;
        for record in [open, data] {
            let mut bytes = vec![0; record.encoded_len().unwrap()];
            record.encode(&mut bytes).unwrap();
            let decoded = Record::decode(&bytes).unwrap();
            state.accept(decoded).unwrap();
            let end = wire.checked_add(u64::try_from(bytes.len()).unwrap()).unwrap();
            assert_ne!(end, decoded.offset);
            assert!(AckCase::Partial.cursor(wire, end, prior_ack).unwrap() < end);
            assert!(AckCase::Forward.cursor(wire, end, prior_ack).unwrap() > end);
            if prior_ack != 0 {
                assert_eq!(AckCase::Replay.cursor(wire, end, prior_ack), Ok(prior_ack));
            }
            wire = end;
            prior_ack = end;
        }
        assert!(matches!(state, StreamState::Open { offset } if offset > 0 && offset < wire));
    }

    #[test]
    fn three_wrong_acks_are_derived_from_the_observed_interval() {
        assert_eq!(AckCase::Partial.cursor(0, 28, 0), Ok(1));
        assert_eq!(AckCase::Replay.cursor(28, 58, 28), Ok(28));
        assert_eq!(AckCase::Forward.cursor(0, 28, 0), Ok(29));
        for case in [AckCase::Partial, AckCase::Replay, AckCase::Forward] {
            assert_eq!(case.cursor(28, 58, 27), Err(Invalid::RecordWindow));
            assert_eq!(case.cursor(28, 28, 28), Err(Invalid::RecordWindow));
        }
        assert_eq!(AckCase::Replay.cursor(0, 28, 0), Err(Invalid::NoPriorAck));
        assert_eq!(AckCase::Partial.cursor(0, 1, 0), Err(Invalid::RecordWindow));
        assert_eq!(
            AckCase::Forward.cursor(u64::MAX - 1, u64::MAX, u64::MAX - 1),
            Err(Invalid::Overflow)
        );
        assert_eq!(AckCase::parse("ack-replay"), Ok(AckCase::Replay));
        assert_eq!(AckCase::parse("cursor-one"), Err(Invalid::Case));
    }

    #[test]
    fn cleanup_and_outer_bounds_share_the_original_start() {
        let began = Instant::now();
        let limits = Limits {
            phase_ms: 3,
            work_ms: 2,
            cleanup_ms: 4,
            outer_ms: 7,
        };
        let at = limits.deadlines(began).unwrap();
        assert_eq!(at.work.duration_since(began), Duration::from_millis(3));
        assert_eq!(at.cleanup.duration_since(began), Duration::from_millis(7));
        assert_eq!(at.cleanup, at.outer);
        assert!(matches!(
            Limits {
                phase_ms: 0,
                ..limits
            }
            .deadlines(began),
            Err(Invalid::ZeroBound)
        ));
        assert!(matches!(
            Limits {
                outer_ms: 6,
                ..limits
            }
            .deadlines(began),
            Err(Invalid::OuterBound)
        ));
        assert!(matches!(
            Limits {
                phase_ms: u64::MAX,
                ..limits
            }
            .deadlines(began),
            Err(Invalid::Overflow)
        ));
    }

    #[test]
    fn post_end_control_selection_keeps_exact_cursor_and_overflow_typed() {
        let end = 102;
        assert_eq!(TerminalCase::Duplicate.cursor(end), Ok(end));
        assert_eq!(TerminalCase::Finish.cursor(end), Ok(end));
        assert_eq!(TerminalCase::Resubscribe.cursor(end), Ok(end));
        assert_eq!(TerminalCase::Forward.cursor(end), Ok(end + 1));
        assert_eq!(TerminalCase::Forward.cursor(u64::MAX), Err(Invalid::Overflow));
        assert_eq!(
            Selection::parse("terminal-end-finish"),
            Ok(Selection::Terminal(TerminalCase::Finish))
        );
        assert_eq!(
            Selection::parse("terminal-end-resubscribe"),
            Ok(Selection::Terminal(TerminalCase::Resubscribe))
        );
        assert_eq!(
            Selection::parse("ack-replay"),
            Ok(Selection::Ack(AckCase::Replay))
        );
        assert_eq!(Selection::parse("terminal-aborted-assumed"), Err(Invalid::Case));
    }
}
