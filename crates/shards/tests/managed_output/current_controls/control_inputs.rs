//! Selected bounded inputs for the existing authenticated control stream.
use super::*;

impl ControlCase {
    pub(super) fn wire(self, observed_end: u64) -> (Vec<u8>, bool, Reason) {
        let ack = managed::consumed(observed_end);
        let header = std::mem::size_of::<u8>() + std::mem::size_of::<u32>();
        let mut bytes = ack.to_vec();
        let mut finish = false;
        let mut reason = Reason::Protocol;
        match self {
            Self::UnknownTag => {
                bytes.truncate(header);
                bytes[0] = u8::MAX;
                bytes[1..header].copy_from_slice(&0u32.to_be_bytes());
                assert!(frame::peek(&bytes).is_err(), "selected tag is actually unknown");
            }
            Self::WrongKnownTag => {
                bytes.clear();
                frame::put(&mut bytes, Tag::Stdout, &[]).unwrap();
                assert_eq!(frame::peek(&bytes).unwrap(), Some((Tag::Stdout, 0)));
            }
            Self::Overdeclared => {
                bytes.truncate(header);
                bytes[1..header].copy_from_slice(&u32::MAX.to_be_bytes());
                assert!(frame::peek(&bytes).is_err());
            }
            Self::AckLength => {
                let short = std::mem::size_of::<u64>().checked_sub(1).unwrap();
                bytes.truncate(header.checked_add(short).unwrap());
                bytes[1..header].copy_from_slice(&u32::try_from(short).unwrap().to_be_bytes());
                assert_eq!(frame::peek(&bytes).unwrap(), Some((Tag::Stdin, short)));
            }
            Self::ShortHeader => {
                bytes.truncate(header.checked_sub(1).unwrap());
                assert_eq!(frame::peek(&bytes).unwrap(), None);
                finish = true;
                reason = Reason::Closed;
            }
            Self::ShortAck => {
                bytes.pop().unwrap();
                assert_eq!(
                    frame::peek(&bytes).unwrap(),
                    Some((Tag::Stdin, std::mem::size_of::<u64>()))
                );
                finish = true;
                reason = Reason::Closed;
            }
            Self::HeaderOnly => {
                bytes.truncate(header);
                assert_eq!(
                    frame::peek(&bytes).unwrap(),
                    Some((Tag::Stdin, std::mem::size_of::<u64>()))
                );
                finish = true;
                reason = Reason::Closed;
            }
        }
        assert!(!bytes.is_empty() && bytes.len() <= managed::CONTROL_BYTES);
        (bytes, finish, reason)
    }
}

#[test]
fn malformed_inputs_keep_actual_buffers_bounded_without_allocating_declared_length() {
    let end = u64::MAX;
    for case in [
        ControlCase::UnknownTag,
        ControlCase::WrongKnownTag,
        ControlCase::Overdeclared,
        ControlCase::AckLength,
        ControlCase::ShortHeader,
        ControlCase::HeaderOnly,
        ControlCase::ShortAck,
    ] {
        let (bytes, finish, reason) = case.wire(end);
        assert!(bytes.len() <= managed::consumed(end).len());
        if matches!(
            case,
            ControlCase::ShortHeader | ControlCase::HeaderOnly | ControlCase::ShortAck
        ) {
            assert!(finish);
            assert_eq!(reason, Reason::Closed);
        } else {
            assert!(!finish);
            assert_eq!(reason, Reason::Protocol);
        }
    }
    let (bytes, _, _) = ControlCase::Overdeclared.wire(end);
    assert_eq!(
        bytes.len(),
        std::mem::size_of::<u8>() + std::mem::size_of::<u32>()
    );
    assert_eq!(
        ControlCase::parse("control-short-ack"),
        Some(ControlCase::ShortAck)
    );
    assert_eq!(ControlCase::parse("cursor-one"), None);
    let (bytes, finish, reason) = ControlCase::HeaderOnly.wire(end);
    assert_eq!(
        bytes.len(),
        std::mem::size_of::<u8>() + std::mem::size_of::<u32>()
    );
    assert!(finish);
    assert_eq!(reason, Reason::Closed);
    assert_eq!(
        ControlCase::parse("control-header-only"),
        Some(ControlCase::HeaderOnly)
    );
}
