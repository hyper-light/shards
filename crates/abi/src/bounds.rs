//! Bounds the VMM and its clients hold to, each derived from what it bounds, never chosen
//! (D12): pure functions of the readings and records their callers pass, so every process
//! derives a bound the same way from its own readings.

/// How many vsock connections a process may hold, each holding `per` of its descriptors
/// (the VMM's muxer one, its host socket; the simulator two, a pair's ends): what its
/// descriptor limit `limit` leaves past the `held` it has open otherwise. None where it holds
/// its limit or more; any number where each holds none.
pub const fn connections(limit: u64, held: u64, per: u64) -> usize {
    let free = limit.saturating_sub(held);
    let n = match free.checked_div(per) {
        Some(n) => n,
        None => u64::MAX,
    };
    if n > usize::MAX as u64 {
        usize::MAX
    } else {
        n as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process's connections are what its descriptor limit leaves past the descriptors it
    /// holds, shared out by those each connection holds; none where it holds its limit or
    /// more, and any number where each holds none.
    #[test]
    fn connections_are_what_the_limit_leaves() {
        assert_eq!(connections(1024, 24, 1), 1000);
        assert_eq!(connections(1024, 24, 2), 500);
        assert_eq!(connections(1024, 25, 2), 499);
        assert_eq!(connections(1024, 1024, 1), 0);
        assert_eq!(connections(1024, 2048, 1), 0);
        assert_eq!(connections(1024, 24, 0), usize::MAX);
    }
}
