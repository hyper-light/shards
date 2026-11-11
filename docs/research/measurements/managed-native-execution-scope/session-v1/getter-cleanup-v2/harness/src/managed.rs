/// The exact kill/events objects opened by checked cgroup admission.
#[derive(Debug)]
pub(crate) struct Controls {
    kill: std::fs::File,
    events: std::fs::File,
}

impl Controls {
    /// Held by the checked cgroup admission producer before any child release.
    /// These objects confer no unused memory, task or future queue reservation.
    pub(crate) fn held(kill: std::fs::File, events: std::fs::File) -> Self {
        Self { kill, events }
    }
}

