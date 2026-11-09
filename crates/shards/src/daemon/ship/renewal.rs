//! How long before they expire a log driver's credentials are found again (D163), learned
//! as it sends, never a figure of an SDK's: a credential must still hold as the service
//! checks it, as long after it is chosen as a request signed with it takes to be answered;
//! and one the service refused as expired while this host's clock gave it longer (a clock
//! behind the service's) must be found again that much sooner too. So the margin is the
//! longest a request has taken from its credential's choosing to its answer, and, where one
//! was refused as expired, as long as it had left then and that request took. Before any
//! request is answered a credential found for it is used however long it has left, and one
//! refused is found again for the next (the drivers' refusals), so that nothing is lost to
//! a margin not yet learned: at worst one request is asked again.

use std::time::{Duration, SystemTime};

/// What a driver has learned of how long before its credentials expire they are found
/// again.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Renewal {
    margin: Duration,
}

impl Renewal {
    /// How long before it expires a credential is found again.
    pub(super) fn margin(&self) -> Duration {
        self.margin
    }

    /// A request answered `took` after its credential was chosen.
    pub(super) fn answered(&mut self, took: Duration) {
        self.margin = self.margin.max(took);
    }

    /// A request refused as its credential had expired, `took` after it was chosen, with
    /// that credential to expire at `expires` by this host's clock as it was chosen, `at`.
    pub(super) fn expired(&mut self, expires: Option<SystemTime>, at: SystemTime, took: Duration) {
        let left = expires
            .and_then(|e| e.duration_since(at).ok())
            .unwrap_or_default();
        self.margin = self.margin.max(left.saturating_add(took));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The margin is the longest answer, and grows past it to what a credential refused as
    /// expired had left as it was chosen, and the time its request took.
    #[test]
    fn the_margin_is_learned_from_answers_and_expiries() {
        let mut r = Renewal::default();
        assert_eq!(r.margin(), Duration::ZERO);
        r.answered(Duration::from_millis(300));
        r.answered(Duration::from_millis(100));
        assert_eq!(r.margin(), Duration::from_millis(300));
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        r.expired(Some(at + Duration::from_secs(90)), at, Duration::from_millis(200));
        assert_eq!(r.margin(), Duration::from_millis(90_200));
        // One already past its expiry by this host's clock teaches only its request's time.
        let mut r = Renewal::default();
        r.expired(Some(at), at + Duration::from_secs(1), Duration::from_millis(50));
        assert_eq!(r.margin(), Duration::from_millis(50));
    }
}
