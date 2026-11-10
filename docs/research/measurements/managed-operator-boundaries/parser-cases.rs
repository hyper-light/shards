#[test]
fn ack_prefixes_and_coalesced_controls_preserve_exact_frame_ends() {
    let first = consumed(u64::MAX);
    for cut in 0..first.len() {
        assert_eq!(input(&first[..cut]).unwrap(), None);
    }
    let second = consumed(0);
    let joined = [first.as_slice(), second.as_slice()].concat();
    let (message, end) = input(&joined).unwrap().unwrap();
    assert_eq!(message, Input::Consumed(u64::MAX));
    assert_eq!(end, first.len());
    assert_eq!(
        input(&joined[end..]).unwrap(),
        Some((Input::Consumed(0), second.len()))
    );
}

#[test]
fn malformed_ack_does_not_resynchronize_into_a_following_valid_control() {
    let valid = consumed(u64::MAX);
    for length in [0, OFFSET_BYTES - 1, OFFSET_BYTES + 1] {
        let mut wrong = consumed(0);
        wrong[1..FRAME_HEAD].copy_from_slice(&u32::try_from(length).unwrap().to_be_bytes());
        let joined = [wrong.as_slice(), valid.as_slice()].concat();
        assert!(matches!(input(&joined), Err(Error::Protocol(ProtocolError::Length))));
    }
    for tag in [0, Tag::Stdout as u8] {
        let mut wrong = consumed(0);
        wrong[0] = tag;
        let joined = [wrong.as_slice(), valid.as_slice()].concat();
        assert!(matches!(input(&joined), Err(Error::Protocol(ProtocolError::ControlKind))));
    }
    assert_eq!(input(&valid).unwrap(), Some((Input::Consumed(u64::MAX), valid.len())));
}

#[test]
fn invalid_subscription_channel_is_not_an_alternate_scope() {
    let scope = Scope {
        run: RunId([42; 32]),
        generation: u64::MAX,
        member: u32::MAX,
        channel: Channel::Stderr,
    };
    let valid = subscribe(scope, u64::MAX);
    let channel_at = FRAME_HEAD + size_of::<RunId>() + size_of::<u64>() + size_of::<u32>();
    for channel in [0, u8::MAX] {
        let mut wrong = valid;
        wrong[channel_at] = channel;
        assert!(matches!(input(&wrong), Err(Error::Protocol(ProtocolError::Channel))));
    }
    assert_eq!(input(&valid).unwrap(), Some((Input::Subscribe(scope, u64::MAX), valid.len())));
}
