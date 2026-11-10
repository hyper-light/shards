//! Test-only encoding reference, not an exporter or a production dependency.

extern crate self as shards_managed_otel_encoder_check;

use std::ffi::OsStr;
use std::io::{self, Write};

pub use native_codec::{Failure, Scope, Snapshot};

mod reference;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    NativeStream,
    OfficialCompactBuffer,
    OfficialPrettyBuffer,
}

impl Mode {
    fn parse(value: &OsStr) -> io::Result<Self> {
        match value.to_str() {
            Some("native-stream") => Ok(Self::NativeStream),
            Some("official-compact-buffer") => Ok(Self::OfficialCompactBuffer),
            Some("official-pretty-buffer") => Ok(Self::OfficialPrettyBuffer),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mode: native-stream | official-compact-buffer | official-pretty-buffer",
            )),
        }
    }

    fn selected() -> io::Result<Self> {
        let mut args = std::env::args_os().skip(1);
        let mode = Self::parse(&args.next().ok_or(io::ErrorKind::InvalidInput)?)?;
        if args.next().is_some() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(mode)
    }
}

// The shared fixture resolves its original crate name to this adapter. Its input
// expressions remain in one file; the native mode invokes the actual root codec.
pub fn write(
    out: &mut impl Write,
    run: &str,
    generation: u64,
    samples: &[Snapshot],
) -> io::Result<()> {
    match Mode::selected()? {
        Mode::NativeStream => native_codec::write(out, run, generation, samples),
        mode => {
            let request = reference::request(run, generation, samples)?;
            let bytes = match mode {
                Mode::OfficialCompactBuffer => serde_json::to_vec(&request),
                // Pinned OTLP HTTP source uses this String -> Vec path. This is
                // its encoding pattern, not an invocation of the SDK pipeline.
                Mode::OfficialPrettyBuffer => {
                    serde_json::to_string_pretty(&request).map(String::into_bytes)
                }
                Mode::NativeStream => return Err(io::ErrorKind::InvalidInput.into()),
            }
            .map_err(io::Error::other)?;
            out.write_all(&bytes)
        }
    }
}

mod concrete {
    include!("../../managed-otel-encoder/fixture.rs");

    pub(super) fn run() -> std::io::Result<()> {
        main()
    }
}

fn main() -> io::Result<()> {
    concrete::run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_explicit_and_do_not_claim_an_sdk_pipeline() {
        for (text, mode) in [
            ("native-stream", Mode::NativeStream),
            ("official-compact-buffer", Mode::OfficialCompactBuffer),
            ("official-pretty-buffer", Mode::OfficialPrettyBuffer),
        ] {
            assert_eq!(Mode::parse(OsStr::new(text)).ok(), Some(mode));
        }
        for text in ["", "sdk", "official-sdk-pretty", "native-stream "] {
            assert!(Mode::parse(OsStr::new(text)).is_err());
        }
    }
}
