//! Pure input and observation checks; this file opens no descriptors.

#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    Arguments,
    Number,
    Positive,
    Overflow,
    RecordLimit,
    Allocation,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Inputs {
    pub ready_counts: Vec<usize>,
    pub rounds: usize,
    pub deadline_ms: u64,
    pub max_records: usize,
}

fn positive(value: &str) -> Result<usize, InputError> {
    let value = value.parse::<usize>().map_err(|_| InputError::Number)?;
    if value == 0 {
        return Err(InputError::Positive);
    }
    Ok(value)
}

impl Inputs {
    /// Positional inputs: ready-counts (comma separated), rounds, deadline-ms,
    /// max-wait-records. There are deliberately no workload defaults.
    pub fn parse(args: &[String]) -> Result<Self, InputError> {
        let [counts, rounds, deadline_ms, max_records] = args else {
            return Err(InputError::Arguments);
        };
        let mut ready_counts = Vec::new();
        for value in counts.split(',') {
            ready_counts
                .try_reserve(1)
                .map_err(|_| InputError::Allocation)?;
            let count = positive(value)?;
            held_descriptors(count, true).ok_or(InputError::Overflow)?;
            u64::try_from(count).map_err(|_| InputError::Overflow)?;
            ready_counts.push(count);
        }
        let rounds = positive(rounds)?;
        let deadline_ms = deadline_ms.parse::<u64>().map_err(|_| InputError::Number)?;
        if deadline_ms == 0 {
            return Err(InputError::Positive);
        }
        let max_records = positive(max_records)?;
        let minimum = ready_counts
            .len()
            .checked_mul(rounds)
            .ok_or(InputError::Overflow)?;
        if max_records < minimum {
            return Err(InputError::RecordLimit);
        }
        Ok(Self {
            ready_counts,
            rounds,
            deadline_ms,
            max_records,
        })
    }
}

pub fn held_descriptors(pairs: usize, poller: bool) -> Option<u64> {
    let streams = pairs.checked_mul(2)?;
    u64::try_from(streams.checked_add(usize::from(poller))?).ok()
}

pub fn acquired_matches(baseline: Option<u64>, acquired: Option<u64>, held: u64) -> bool {
    matches!((baseline, acquired), (Some(base), Some(now)) if base.checked_add(held) == Some(now))
}

pub fn returned(baseline: Option<u64>, released: Option<u64>) -> bool {
    matches!((baseline, released), (Some(base), Some(now)) if base == now)
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    Allocation,
    Foreign(u64),
    Duplicate(u64),
    Missing(usize),
}

#[derive(Debug)]
pub struct Round {
    seen: Vec<bool>,
    remaining: usize,
}

impl Round {
    pub fn new(count: usize) -> Result<Self, TokenError> {
        let mut seen = Vec::new();
        seen.try_reserve_exact(count)
            .map_err(|_| TokenError::Allocation)?;
        seen.resize(count, false);
        Ok(Self {
            seen,
            remaining: count,
        })
    }

    pub fn observe(&mut self, token: u64) -> Result<usize, TokenError> {
        let index = usize::try_from(token).map_err(|_| TokenError::Foreign(token))?;
        let seen = self.seen.get_mut(index).ok_or(TokenError::Foreign(token))?;
        if *seen {
            return Err(TokenError::Duplicate(token));
        }
        *seen = true;
        self.remaining -= 1; // One previously unseen member of this exact finite set.
        Ok(index)
    }

    pub fn complete(&self) -> Result<(), TokenError> {
        if self.remaining == 0 {
            Ok(())
        } else {
            Err(TokenError::Missing(self.remaining))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Inputs, InputError> {
        Inputs::parse(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn explicit_counts_rounds_deadline_and_record_limit_are_retained() {
        let inputs = parse(&["1,64,65,1", "3", "9000", "12"]).unwrap();
        assert_eq!(inputs.ready_counts, [1, 64, 65, 1]);
        assert_eq!(
            (inputs.rounds, inputs.deadline_ms, inputs.max_records),
            (3, 9000, 12)
        );
    }

    #[test]
    fn malformed_missing_zero_and_insufficient_inputs_reject() {
        for args in [vec![], vec!["1", "2", "3"], vec!["1", "2", "3", "4", "5"]] {
            assert_eq!(parse(&args), Err(InputError::Arguments));
        }
        for args in [
            ["", "1", "1", "1"],
            ["1,", "1", "1", "2"],
            ["x", "1", "1", "1"],
            ["1", "-1", "1", "1"],
            ["1", "1", "x", "1"],
        ] {
            assert_eq!(parse(&args), Err(InputError::Number));
        }
        for args in [
            ["0", "1", "1", "1"],
            ["1", "0", "1", "1"],
            ["1", "1", "0", "1"],
            ["1", "1", "1", "0"],
        ] {
            assert_eq!(parse(&args), Err(InputError::Positive));
        }
        assert_eq!(parse(&["1,2", "2", "1", "3"]), Err(InputError::RecordLimit));
    }

    #[test]
    fn descriptor_and_record_arithmetic_refuses_overflow() {
        let largest = usize::MAX.to_string();
        assert_eq!(held_descriptors(usize::MAX, true), None);
        assert_eq!(parse(&[&largest, "1", "1", "1"]), Err(InputError::Overflow));
        assert_eq!(
            parse(&["1,2", &largest, "1", &largest]),
            Err(InputError::Overflow)
        );
    }

    #[test]
    fn token_rejections_preserve_remaining_members_and_rounds_reset() {
        let mut round = Round::new(3).unwrap();
        assert_eq!(round.observe(2), Ok(2));
        assert_eq!(round.complete(), Err(TokenError::Missing(2)));
        assert_eq!(round.observe(2), Err(TokenError::Duplicate(2)));
        assert_eq!(round.observe(3), Err(TokenError::Foreign(3)));
        assert_eq!(round.observe(u64::MAX), Err(TokenError::Foreign(u64::MAX)));
        assert_eq!(round.complete(), Err(TokenError::Missing(2)));
        assert_eq!(round.observe(0), Ok(0));
        assert_eq!(round.observe(1), Ok(1));
        assert_eq!(round.complete(), Ok(()));
        let mut next = Round::new(3).unwrap();
        assert_eq!(next.observe(2), Ok(2));
        assert_eq!(next.complete(), Err(TokenError::Missing(2)));
    }

    #[test]
    fn fd_return_requires_known_exact_acquisition_and_release() {
        assert_eq!(held_descriptors(3, true), Some(7));
        assert!(acquired_matches(Some(5), Some(12), 7));
        assert!(returned(Some(5), Some(5)));
        assert!(!acquired_matches(Some(5), Some(11), 7));
        assert!(!acquired_matches(Some(u64::MAX), Some(0), 1));
        assert!(!returned(Some(5), Some(6)));
        assert!(!returned(None, None));
        assert!(!acquired_matches(None, None, 0));
    }
}
