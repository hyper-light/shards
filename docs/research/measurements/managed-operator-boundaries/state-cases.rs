#[test]
fn decoded_last_legal_ack_preserves_pending_bytes_until_the_exact_end() {
    let bytes = [0, u8::MAX];
    let start = u64::MAX - u64::try_from(bytes.len()).unwrap();
    for ending in [Ending::Open, Ending::Finishing] {
        let service = service(Slot::AwaitingAck { start, bytes: bytes.to_vec() });
        lock(&service.shared).1 = ending;
        let pointer = match &lock(&service.shared).0 {
            Slot::AwaitingAck { bytes, .. } => bytes.as_ptr(),
            other => panic!("unexpected prepared slot {other:?}"),
        };
        for cursor in [0, start, u64::MAX - 1] {
            let wire = consumed(cursor);
            let (Input::Consumed(decoded), end) = input(&wire).unwrap().unwrap() else {
                panic!("expected ACK wire variant");
            };
            assert_eq!(end, wire.len());
            assert!(!service.consumed(decoded));
            let state = lock(&service.shared);
            assert_eq!(progress(&state.0), Progress { accepted: u64::MAX, consumed: start });
            match &state.0 {
                Slot::AwaitingAck { start: actual, bytes: retained } => {
                    assert_eq!(*actual, start);
                    assert_eq!(retained, &bytes);
                    assert_eq!(retained.as_ptr(), pointer);
                }
                other => panic!("rejected ACK changed pending record {other:?}"),
            }
        }
        let wire = consumed(u64::MAX);
        let (Input::Consumed(decoded), _) = input(&wire).unwrap().unwrap() else {
            panic!("expected final ACK");
        };
        assert!(service.consumed(decoded));
        let state = lock(&service.shared);
        match ending {
            Ending::Open => assert!(matches!(state.0, Slot::Ready(u64::MAX))),
            Ending::Finishing => assert!(matches!(state.0, Slot::Finished(u64::MAX))),
            Ending::Aborted => panic!("not a prepared case"),
        }
        assert_eq!(progress(&state.0), Progress { accepted: u64::MAX, consumed: u64::MAX });
    }
}

#[test]
fn malformed_control_then_disconnect_requires_replay_before_ack() {
    let bytes = vec![0, u8::MAX];
    let pointer = bytes.as_ptr();
    let start = u64::MAX - u64::try_from(bytes.len()).unwrap();
    let mut service = service(Slot::AwaitingAck { start, bytes });
    let mut malformed = consumed(u64::MAX);
    malformed[0] = Tag::Stdout as u8;
    assert!(matches!(input(&malformed), Err(Error::Protocol(ProtocolError::ControlKind))));
    // Service::refuse_reason performs this existing disconnect after its I/O close.
    // This pure composition deliberately does not model or claim that I/O close.
    service.disconnect();
    assert!(matches!(service.state, Subscription::Listening));
    match &lock(&service.shared).0 {
        Slot::Record { start: actual, bytes, written: 0 } => {
            assert_eq!(*actual, start);
            assert_eq!(bytes.as_ptr(), pointer);
        }
        other => panic!("refusal cleanup must retain the unsent replay {other:?}"),
    }
    assert!(!service.subscribe(u64::MAX));
    assert!(service.subscribe(start));
    let (Input::Consumed(end), _) = input(&consumed(u64::MAX)).unwrap().unwrap() else {
        panic!("expected ACK");
    };
    assert!(!service.consumed(end), "a replay has not yet been sent");
    assert!(matches!(lock(&service.shared).0, Slot::Record { written: 0, .. }));
}
