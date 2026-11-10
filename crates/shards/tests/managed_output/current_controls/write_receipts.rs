//! Accounting for actual write receipts; never models an endpoint or peer ACK.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Invalid {
    Empty,
    Overflow,
    Accepted,
    Pending,
    Incomplete,
}

#[derive(Debug)]
pub(crate) struct Writes {
    admitted: usize,
    accepted: usize,
}

impl Writes {
    pub(crate) fn new(admitted: usize) -> Result<Self, Invalid> {
        if admitted == 0 {
            return Err(Invalid::Empty);
        }
        Ok(Self {
            admitted,
            accepted: 0,
        })
    }

    pub(crate) fn accept(&mut self, count: usize, pending: usize) -> Result<(), Invalid> {
        if count == 0 {
            return Err(Invalid::Accepted);
        }
        let accepted = self.accepted.checked_add(count).ok_or(Invalid::Overflow)?;
        let remaining = self.admitted.checked_sub(accepted).ok_or(Invalid::Accepted)?;
        if pending != remaining {
            return Err(Invalid::Pending);
        }
        self.accepted = accepted;
        Ok(())
    }

    pub(crate) fn complete(&self) -> Result<usize, Invalid> {
        if self.accepted != self.admitted {
            return Err(Invalid::Incomplete);
        }
        Ok(self.accepted)
    }
}

#[test]
fn receipts_require_actual_whole_write_and_preserve_partial_accounting_on_rejection() {
    let mut writes = Writes::new(13).unwrap();
    writes.accept(5, 8).unwrap();
    assert_eq!(writes.complete(), Err(Invalid::Incomplete));
    assert_eq!(writes.accept(0, 8), Err(Invalid::Accepted));
    assert_eq!(writes.accept(8, 1), Err(Invalid::Pending));
    writes.accept(8, 0).unwrap();
    assert_eq!(writes.complete(), Ok(13));
    assert_eq!(writes.accept(1, 0), Err(Invalid::Accepted));
    assert_eq!(writes.complete(), Ok(13));
    assert!(matches!(Writes::new(0), Err(Invalid::Empty)));
}

#[test]
fn accepted_count_overflow_never_commits_a_receipt() {
    let mut writes = Writes::new(usize::MAX).unwrap();
    writes.accept(usize::MAX - 1, 1).unwrap();
    assert_eq!(writes.accept(2, 0), Err(Invalid::Overflow));
    assert_eq!(writes.complete(), Err(Invalid::Incomplete));
    writes.accept(1, 0).unwrap();
    assert_eq!(writes.complete(), Ok(usize::MAX));
}
