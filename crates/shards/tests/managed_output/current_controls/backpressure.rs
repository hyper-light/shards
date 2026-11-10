//! Checks retained actual I/O receipts; it never drives or models a transport.
use super::ControlReceipt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Invalid {
    Empty,
    Order,
    Backlog,
    Count,
    Readiness,
    WriteError,
    Incomplete,
    NoZero,
    NoResumption,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Summary {
    accepted: usize,
    zero_returns: usize,
    readiness: usize,
    resumed_positive: usize,
}

#[derive(Clone, Copy)]
enum Phase<'a> {
    Ready,
    Attempt { offered: &'a [u8], resumed: bool },
    Blocked,
    Notified,
}

pub(super) fn check(receipts: &[ControlReceipt], packet: &[u8]) -> Result<Summary, Invalid> {
    if packet.is_empty() {
        return Err(Invalid::Empty);
    }
    let mut pending = packet;
    let mut phase = Phase::Ready;
    let mut summary = Summary {
        accepted: 0,
        zero_returns: 0,
        readiness: 0,
        resumed_positive: 0,
    };
    for receipt in receipts {
        match receipt {
            ControlReceipt::WriteAttempt { pending: offered } => {
                let resumed = match phase {
                    Phase::Ready => false,
                    Phase::Notified => true,
                    Phase::Blocked => return Err(Invalid::Readiness),
                    Phase::Attempt { .. } => return Err(Invalid::Order),
                };
                if offered.is_empty() || offered != pending {
                    return Err(Invalid::Backlog);
                }
                phase = Phase::Attempt { offered, resumed };
            }
            ControlReceipt::WriteResult {
                accepted,
                pending: after,
            } => {
                let Phase::Attempt { offered, resumed } = phase else {
                    return Err(Invalid::Order);
                };
                let remaining = offered.get(*accepted..).ok_or(Invalid::Count)?;
                if after != remaining {
                    return Err(Invalid::Backlog);
                }
                pending = remaining;
                summary.accepted = summary.accepted.checked_add(*accepted).ok_or(Invalid::Count)?;
                if *accepted == 0 {
                    summary.zero_returns += 1;
                    phase = Phase::Blocked;
                } else {
                    summary.resumed_positive += usize::from(resumed);
                    phase = Phase::Ready;
                }
            }
            ControlReceipt::Writable {
                owned_connection,
                owned_stream,
                pending: offered,
            } => {
                if !owned_connection || !owned_stream {
                    return Err(Invalid::Readiness);
                }
                if offered != pending {
                    return Err(Invalid::Backlog);
                }
                match phase {
                    Phase::Attempt { .. } => return Err(Invalid::Order),
                    Phase::Blocked => {
                        summary.readiness += 1;
                        phase = Phase::Notified;
                    }
                    Phase::Ready | Phase::Notified => {}
                }
            }
            ControlReceipt::WriteError(_) => return Err(Invalid::WriteError),
            ControlReceipt::Write { .. } | ControlReceipt::Read { .. } | ControlReceipt::Finish { .. } => {}
        }
    }
    if !pending.is_empty() || !matches!(phase, Phase::Ready) || summary.accepted != packet.len() {
        return Err(Invalid::Incomplete);
    }
    if summary.zero_returns == 0 {
        return Err(Invalid::NoZero);
    }
    if summary.resumed_positive == 0 {
        return Err(Invalid::NoResumption);
    }
    Ok(summary)
}

#[test]
fn explicit_zero_requires_unchanged_backlog_and_owned_readiness_before_positive() {
    use ControlReceipt::{Writable, WriteAttempt, WriteResult};
    let packet = b"abc";
    let mut receipts = vec![
        WriteAttempt {
            pending: packet.to_vec(),
        },
        WriteResult {
            accepted: 1,
            pending: b"bc".to_vec(),
        },
        WriteAttempt {
            pending: b"bc".to_vec(),
        },
        WriteResult {
            accepted: 0,
            pending: b"bc".to_vec(),
        },
        Writable {
            owned_connection: true,
            owned_stream: true,
            pending: b"bc".to_vec(),
        },
        WriteAttempt {
            pending: b"bc".to_vec(),
        },
        WriteResult {
            accepted: 2,
            pending: Vec::new(),
        },
    ];
    assert_eq!(
        check(&receipts, packet),
        Ok(Summary {
            accepted: 3,
            zero_returns: 1,
            readiness: 1,
            resumed_positive: 1
        })
    );
    receipts.remove(4);
    assert_eq!(check(&receipts, packet), Err(Invalid::Readiness));
    receipts[3] = WriteResult {
        accepted: 0,
        pending: b"c".to_vec(),
    };
    assert_eq!(check(&receipts, packet), Err(Invalid::Backlog));
}

#[test]
fn positive_only_or_incomplete_receipts_cannot_establish_zero_coverage() {
    use ControlReceipt::{WriteAttempt, WriteResult};
    let mut receipts = vec![
        WriteAttempt {
            pending: b"abc".to_vec(),
        },
        WriteResult {
            accepted: 3,
            pending: Vec::new(),
        },
    ];
    assert_eq!(check(&receipts, b"abc"), Err(Invalid::NoZero));
    receipts[1] = WriteResult {
        accepted: 0,
        pending: b"abc".to_vec(),
    };
    assert_eq!(check(&receipts, b"abc"), Err(Invalid::Incomplete));
    receipts[1] = WriteResult {
        accepted: 4,
        pending: Vec::new(),
    };
    assert_eq!(check(&receipts, b"abc"), Err(Invalid::Count));
    receipts.remove(0);
    assert_eq!(check(&receipts, b"abc"), Err(Invalid::Order));
}
