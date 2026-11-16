//! The pause between looks at something that changes unannounced (D215): `p = sqrt(2 c w)`,
//! one look's own cost `c` against the wait so far `w`, both measured where the look runs.
//! Shared by the host and the guest (R12): D215's dials and waits, D188's looks at a
//! budget's room and at a host's pressure that goes on, and D133's grower's looks for its
//! memory.
//!
//! - `w` is how long the wait has gone on so far, and stands for the wait still to come.
//! - Pausing `p` between looks, a wait of `w` takes `w / p` looks, `c w / p` of looking; and
//!   what is looked for, come at a uniform moment within a pause, is seen `p / 2` late on
//!   average. Both are seconds, of the looker and of whoever waits on it.
//! - Their sum `f(p) = c w / p + p / 2` is least where `f'(p) = -c w / p² + 1 / 2 = 0`, that
//!   is `p² = 2 c w`: `p = sqrt(2 c w)`, Young's optimum interval between checkpoints
//!   (CACM 17(9), 1974) with the wait so far for the mean time to the event.
//!
//! So a few looks go out early and fewer as the wait grows: none waited, none paused (the
//! cold start looks at once); a look's cost of none, none paused either, as looking costs
//! nothing. The product saturates at u128's most, the pause at u64's nanoseconds.

use core::time::Duration;

/// `sqrt(2 c w)` of a look's cost `cost` and the wait so far `waited`, at most `left`.
pub fn pause(cost: Duration, waited: Duration, left: Duration) -> Duration {
    let product = cost
        .as_nanos()
        .checked_mul(waited.as_nanos())
        .and_then(|p| p.checked_mul(2))
        .unwrap_or(u128::MAX);
    let nanos = u64::try_from(product.isqrt()).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos).min(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pause is `sqrt(2 c w)`, none at first, never past `left`, and saturates.
    #[test]
    fn a_pause_weighs_a_looks_cost_against_the_wait_so_far() {
        let (us, ms) = (Duration::from_micros, Duration::from_millis);
        assert_eq!(pause(us(5), Duration::ZERO, Duration::MAX), Duration::ZERO);
        assert_eq!(pause(Duration::ZERO, ms(150), Duration::MAX), Duration::ZERO);
        assert_eq!(
            pause(us(5), ms(150), Duration::MAX),
            Duration::from_nanos(1_224_744)
        );
        assert_eq!(
            pause(us(50), ms(150), Duration::MAX),
            Duration::from_nanos(3_872_983)
        );
        assert_eq!(pause(us(5), ms(150), ms(1)), ms(1));
        assert_eq!(pause(Duration::MAX, Duration::MAX, ms(1)), ms(1));
        assert_eq!(
            pause(Duration::MAX, Duration::MAX, Duration::MAX),
            Duration::from_nanos(u64::MAX)
        );
    }
}
