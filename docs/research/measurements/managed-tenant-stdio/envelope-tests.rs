use super::*;

#[test]
fn host_guest_and_actual_payload_framing_envelopes_agree_without_challenge_allocation() {
    let actual_count = std::env::var("SHARDS_ENVELOPE_CHALLENGE_BYTES")
        .expect("explicit count from the ROOT actual control exchange")
        .parse::<u32>()
        .unwrap();
    assert!(actual_count > 0);
    for count in [1, actual_count, u32::MAX] {
        let capture = ["agent", "agent-inherited-ready", "agent-inherited-after-release"]
            .into_iter()
            .flat_map(|name| [payload(name, Channel::Stdout), payload(name, Channel::Stderr)])
            .map(|bytes| u64::try_from(bytes.len()).unwrap())
            .sum::<u64>();
        let size = count.to_be_bytes();
        let header = [&[1][..], &size].concat();
        let one_frame = u64::try_from(header.len())
            .unwrap()
            .checked_add(u64::from(count))
            .unwrap();
        let actual_layout = capture.checked_add(one_frame.checked_mul(2).unwrap()).unwrap();
        let host = u64::try_from(inherited_report_bytes(usize::try_from(count).unwrap()).unwrap()).unwrap();
        let guest = managed_capture_inherited_required("agent", count).unwrap();
        assert_eq!(host, actual_layout);
        assert_eq!(guest, actual_layout);
    }
}

#[test]
fn host_count_overflow_refuses_and_zero_count_calculations_agree() {
    assert_eq!(inherited_report_bytes(usize::MAX), None);
    let zero_host = u64::try_from(inherited_report_bytes(0).unwrap()).unwrap();
    let zero_guest = managed_capture_inherited_required("agent", 0).unwrap();
    assert_eq!(zero_host, zero_guest);
    // Count zero is defined arithmetic only; Supplied/guest mode admission still rejects it.
    let max_guest = managed_capture_inherited_required("agent", u32::MAX).unwrap();
    assert_eq!(
        max_guest.checked_sub(zero_guest),
        u64::from(u32::MAX).checked_mul(2)
    );
}
