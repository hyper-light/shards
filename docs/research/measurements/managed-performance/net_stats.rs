//! Test-only snapshot of the exact Driver connection; no production API/state.

use super::{ConnKey, Io};

impl Io<'_> {
    pub(crate) fn measurement_stats(&mut self, conn: ConnKey) -> Option<hyper_quic::ConnectionStats> {
        Some(self.conn(conn).ok()?.quic.stats())
    }
}
