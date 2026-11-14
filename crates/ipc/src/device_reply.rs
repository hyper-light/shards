//! Device outcomes shared by the VMM, VM process and daemon (D214).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    NoBus,
    NoSlots { slots: u64 },
    NoDevice { devfn: u8 },
    StillDriven { devfn: u8, reason: String },
    Shrink { has: u64, asked: u64 },
    Unaligned { bytes: u64, sector: u64 },
    Inactive,
    InvalidRequest(String),
    Host(String),
    Unsupported(String),
    ConnectionEnded,
    InvalidReply,
    Legacy(String),
}

/// A lost answer may follow a completed operation: callers must reconcile, not retry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    AfterRelease,
    CorrectRequest,
    Reconcile,
    Unknown,
}

impl Refusal {
    pub fn retry(&self) -> Retry {
        match self {
            Self::NoSlots { .. } | Self::StillDriven { .. } => Retry::AfterRelease,
            Self::Host(_) | Self::Legacy(_) => Retry::Unknown,
            Self::ConnectionEnded | Self::InvalidReply => Retry::Reconcile,
            _ => Retry::CorrectRequest,
        }
    }
}

impl From<String> for Refusal {
    fn from(error: String) -> Self {
        Self::Host(error)
    }
}

impl From<Refusal> for String {
    fn from(error: Refusal) -> Self {
        error.to_string()
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBus => f.write_str("this microVM has no PCI slots"),
            Self::NoSlots { slots } => write!(f, "every one of its {slots} hot-plug slots holds a function"),
            Self::NoDevice { devfn } => write!(f, "no device is plugged at devfn {devfn:#04x}"),
            Self::StillDriven { devfn, reason } => write!(f, "device {devfn:#04x}: {reason}"),
            Self::Shrink { has, asked } => write!(f, "its file shrank to {asked} bytes from {has}: a disk only grows under its guest"),
            Self::Unaligned { bytes, sector } => write!(f, "its file of {bytes} bytes: not whole sectors of {sector}"),
            Self::Inactive => f.write_str("the microVM is not running"),
            Self::InvalidRequest(why) | Self::Host(why) | Self::Unsupported(why) | Self::Legacy(why) => f.write_str(why),
            Self::ConnectionEnded => f.write_str("its microVM stopped before answering; the operation may have completed"),
            Self::InvalidReply => f.write_str("its microVM sent an invalid device answer; the operation may have completed"),
        }
    }
}

/// DEVICED's status and body, excluding its request number. Status 1 is the legacy
/// prose-only refusal, deliberately never inferred back into a cause.
pub fn encode(answer: &Result<String, Refusal>) -> Result<Vec<u8>, std::collections::TryReserveError> {
    let (status, text) = match answer {
        Ok(text) => (0, text.clone()),
        Err(error) => (1, error.to_string()),
    };
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(1usize.saturating_add(text.len()))?;
    bytes.push(status);
    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<String, Refusal> {
    let (&status, rest) = bytes.split_first().ok_or(Refusal::InvalidReply)?;
    let text = std::str::from_utf8(rest).map_err(|_| Refusal::InvalidReply)?.to_string();
    match status {
        0 => Ok(text),
        1 => Err(Refusal::Legacy(text)),
        _ => Err(Refusal::InvalidReply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_refusals_keep_identity_and_retry_across_the_wire() {
        for refusal in [
            Refusal::NoSlots { slots: 4 },
            Refusal::StillDriven { devfn: 8, reason: "driver active".into() },
            Refusal::Shrink { has: 1024, asked: 512 },
            Refusal::Host("VFIO map failed".into()),
        ] {
            let expected_retry = refusal.retry();
            let decoded = decode(&encode(&Err(refusal.clone())).unwrap()).unwrap_err();
            assert_eq!(decoded.retry(), expected_retry);
            assert_eq!(decoded, refusal);
        }
    }
}
