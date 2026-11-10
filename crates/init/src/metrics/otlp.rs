//! OTLP 1.11.1 JSON for init's cgroup observations. Protocol definitions and the
//! adoption gates are recorded in managed-otel-encoder/README.md under measurements.

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Io(i32),
    Missing,
    Invalid,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Run,
    Workload,
    Member(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub scope: Scope,
    pub time_unix_nano: u64,
    pub cpu_usec: Result<u64, Failure>,
    pub memory_bytes: Result<u64, Failure>,
    /// cgroup pids.current counts subtree TIDs, not just process leaders.
    pub threads: Result<u64, Failure>,
    pub read_bytes: Result<u64, Failure>,
    pub write_bytes: Result<u64, Failure>,
}

/// One ExportMetricsServiceRequest, with cumulative kernel values. Unknown counter
/// birth times are omitted, not replaced with each collection's timestamp.
/// `generation` is the VMM lineage restore count, not a management epoch.
pub fn write(
    out: &mut impl Write,
    run: &str,
    microvm: &str,
    generation: u64,
    samples: &[Snapshot],
) -> io::Result<()> {
    if samples.iter().any(|s| s.time_unix_nano == 0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    out.write_all(b"{\"resourceMetrics\":[")?;
    for (at, sample) in samples.iter().enumerate() {
        if at != 0 {
            out.write_all(b",")?;
        }
        resource(out, run, microvm, generation, sample)?;
    }
    out.write_all(b"]}\n")
}

fn resource(
    out: &mut impl Write,
    run: &str,
    microvm: &str,
    generation: u64,
    sample: &Snapshot,
) -> io::Result<()> {
    out.write_all(b"{\"resource\":{\"attributes\":[")?;
    attribute(out, "shards.run.id", run)?;
    out.write_all(b",")?;
    attribute(out, "shards.microvm.id", microvm)?;
    // The VMM lineage restore count is u64; AnyValue.int_value is int64.
    write!(
        out,
        ",{{\"key\":\"shards.microvm.restore.generation\",\"value\":{{\"stringValue\":\"{generation}\"}}}},"
    )?;
    let scope = match sample.scope {
        Scope::Run => "run",
        Scope::Workload => "workload",
        Scope::Member(_) => "member",
    };
    attribute(out, "shards.cgroup.scope", scope)?;
    if let Scope::Member(member) = sample.scope {
        write!(
            out,
            ",{{\"key\":\"shards.member.id\",\"value\":{{\"intValue\":\"{member}\"}}}}"
        )?;
    }
    out.write_all(b"]},\"scopeMetrics\":[{\"scope\":{\"name\":\"shards.init\"},\"metrics\":[")?;
    let points = observations(sample);
    for instrument in [
        Instrument::Cpu,
        Instrument::Memory,
        Instrument::Tasks,
        Instrument::Io,
    ] {
        metric(out, sample.time_unix_nano, instrument, &points)?;
    }
    diagnostics(out, sample.time_unix_nano, &points)?;
    out.write_all(b"]}]}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Instrument {
    Cpu,
    Memory,
    Tasks,
    Io,
}

struct Observation {
    instrument: Instrument,
    direction: Option<&'static str>,
    value: Result<u64, Failure>,
}

fn observations(sample: &Snapshot) -> [Observation; 5] {
    let point = |instrument, direction, value: Result<u64, Failure>| Observation {
        instrument,
        direction,
        value: if instrument == Instrument::Cpu {
            value
        } else {
            value.and_then(|n| i64::try_from(n).map(|_| n).map_err(|_| Failure::Overflow))
        },
    };
    [
        point(Instrument::Cpu, None, sample.cpu_usec),
        point(Instrument::Memory, None, sample.memory_bytes),
        point(Instrument::Tasks, None, sample.threads),
        point(Instrument::Io, Some("read"), sample.read_bytes),
        point(Instrument::Io, Some("write"), sample.write_bytes),
    ]
}

impl Instrument {
    fn fields(self) -> (&'static str, &'static str, bool) {
        match self {
            Self::Cpu => ("shards.cgroup.cpu.time", "s", true),
            Self::Memory => ("shards.cgroup.memory.usage", "By", false),
            Self::Tasks => ("shards.cgroup.task.count", "{task}", false),
            Self::Io => ("shards.cgroup.disk.io", "By", true),
        }
    }
}

fn metric(
    out: &mut impl Write,
    timestamp: u64,
    instrument: Instrument,
    points: &[Observation],
) -> io::Result<()> {
    let recorded = || {
        points
            .iter()
            .filter(|p| p.instrument == instrument && p.value.is_ok())
    };
    if recorded().next().is_none() {
        return Ok(());
    }
    let (name, unit, monotonic) = instrument.fields();
    write!(
        out,
        "{{\"name\":\"{name}\",\"unit\":\"{unit}\",\"sum\":{{\"aggregationTemporality\":2,\"isMonotonic\":{monotonic},\"dataPoints\":["
    )?;
    for (at, point) in recorded().enumerate() {
        if at != 0 {
            out.write_all(b",")?;
        }
        write!(out, "{{\"timeUnixNano\":\"{timestamp}\",\"attributes\":[")?;
        if let Some(direction) = point.direction {
            attribute(out, "disk.io.direction", direction)?;
        }
        out.write_all(b"]")?;
        match point.value {
            Ok(usec) if matches!(instrument, Instrument::Cpu) => {
                // SI: one second is 10^6 microseconds. JSON decodes this decimal as
                // binary64; this does not promise exact preservation of large usec.
                const PER_SECOND: u64 = 1_000_000;
                write!(
                    out,
                    ",\"asDouble\":{}.{:06}",
                    usec / PER_SECOND,
                    usec % PER_SECOND
                )?;
            }
            Ok(value) => write!(out, ",\"asInt\":\"{value}\"")?,
            Err(_) => return Err(io::ErrorKind::InvalidData.into()),
        }
        out.write_all(b"}")?;
    }
    out.write_all(b"]}},")
}

fn diagnostics(out: &mut impl Write, timestamp: u64, points: &[Observation]) -> io::Result<()> {
    let failures = points.iter().filter(|p| p.value.is_err()).count();
    write!(
        out,
        "{{\"name\":\"shards.cgroup.collection.error.count\",\"unit\":\"{{error}}\",\"gauge\":{{\"dataPoints\":[{{\"timeUnixNano\":\"{timestamp}\",\"asInt\":\"{failures}\"}}]}}}}"
    )?;
    if failures == 0 {
        return Ok(());
    }
    out.write_all(
        b",{\"name\":\"shards.cgroup.collection.error\",\"unit\":\"{error}\",\"gauge\":{\"dataPoints\":[",
    )?;
    for (at, point) in points.iter().filter(|p| p.value.is_err()).enumerate() {
        if at != 0 {
            out.write_all(b",")?;
        }
        let cause = match point.value {
            Err(cause) => cause,
            Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
        };
        write!(
            out,
            "{{\"timeUnixNano\":\"{timestamp}\",\"asInt\":\"1\",\"attributes\":["
        )?;
        attribute(out, "shards.cgroup.metric", point.instrument.fields().0)?;
        if let Some(direction) = point.direction {
            out.write_all(b",")?;
            attribute(out, "disk.io.direction", direction)?;
        }
        out.write_all(b",")?;
        let kind = match cause {
            Failure::Io(_) => "io",
            Failure::Missing => "missing",
            Failure::Invalid => "invalid",
            Failure::Overflow => "overflow",
        };
        attribute(out, "error.type", kind)?;
        if let Failure::Io(errno) = cause {
            write!(
                out,
                ",{{\"key\":\"shards.error.errno\",\"value\":{{\"intValue\":\"{errno}\"}}}}"
            )?;
        }
        out.write_all(b"]}")?;
    }
    out.write_all(b"]}}")
}

fn attribute(out: &mut impl Write, key: &str, value: &str) -> io::Result<()> {
    out.write_all(b"{\"key\":")?;
    string(out, key)?;
    out.write_all(b",\"value\":{\"stringValue\":")?;
    string(out, value)?;
    out.write_all(b"}}")
}

fn string(out: &mut impl Write, text: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let mut start = 0;
    for (at, character) in text.char_indices() {
        if character > '\u{1f}' && character != '"' && character != '\\' {
            continue;
        }
        out.write_all(text.get(start..at).ok_or(io::ErrorKind::InvalidInput)?.as_bytes())?;
        match character {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            control => write!(out, "\\u{:04x}", u32::from(control))?,
        }
        start = at
            .checked_add(character.len_utf8())
            .ok_or(io::ErrorKind::InvalidInput)?;
    }
    out.write_all(text.get(start..).ok_or(io::ErrorKind::InvalidInput)?.as_bytes())?;
    out.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            scope: Scope::Run,
            time_unix_nano: u64::MAX,
            cpu_usec: Ok(1_000_001),
            memory_bytes: Ok(0),
            threads: Ok(4),
            read_bytes: Ok(12),
            write_bytes: Ok(34),
        }
    }

    fn encode(samples: &[Snapshot]) -> String {
        let mut bytes = Vec::new();
        write(&mut bytes, "run", "vm", u64::MAX, samples).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn scopes_and_exact_identities_stay_distinct() {
        let mut run = sample();
        let mut workload = sample();
        workload.scope = Scope::Workload;
        let mut member = sample();
        member.scope = Scope::Member(u32::MAX);
        let text = encode(&[run, workload, member]);
        assert_eq!(
            text.matches("\"shards.run.id\",\"value\":{\"stringValue\":\"run\"}")
                .count(),
            3
        );
        assert_eq!(
            text.matches("\"shards.microvm.id\",\"value\":{\"stringValue\":\"vm\"}")
                .count(),
            3
        );
        for name in ["run", "workload", "member"] {
            assert!(text.contains(&format!(
                "\"shards.cgroup.scope\",\"value\":{{\"stringValue\":\"{name}\"}}"
            )));
        }
        assert_eq!(
            text.matches(
                "\"shards.microvm.restore.generation\",\"value\":{\"stringValue\":\"18446744073709551615\"}"
            )
            .count(),
            3
        );
        assert_eq!(text.matches("\"shards.member.id\"").count(), 1);
        assert!(text.contains("\"intValue\":\"4294967295\""));
        assert!(!text.contains("startTimeUnixNano"));
        assert!(!text.contains("\"shards.microvm.generation\""));
        run.time_unix_nano = 1;
        assert!(encode(&[run]).contains("\"timeUnixNano\":\"1\""));
    }

    #[test]
    fn counts_units_temporality_and_directions_match_the_contract() {
        let text = encode(&[sample()]);
        assert_eq!(text.matches("\"aggregationTemporality\":2").count(), 4);
        assert_eq!(text.matches("\"isMonotonic\":true").count(), 2);
        assert_eq!(text.matches("\"isMonotonic\":false").count(), 2);
        assert!(text.contains("\"shards.cgroup.cpu.time\",\"unit\":\"s\""));
        assert!(text.contains("\"shards.cgroup.memory.usage\",\"unit\":\"By\""));
        assert!(text.contains("\"shards.cgroup.task.count\",\"unit\":\"{task}\""));
        assert!(text.contains("\"shards.cgroup.disk.io\",\"unit\":\"By\""));
        assert!(text.contains("\"disk.io.direction\",\"value\":{\"stringValue\":\"read\"}"));
        assert!(text.contains("\"disk.io.direction\",\"value\":{\"stringValue\":\"write\"}"));
        assert!(text.contains("\"asInt\":\"0\""));
        assert!(text.contains("\"asInt\":\"12\""));
        assert!(text.contains("\"asInt\":\"34\""));
        assert!(!text.contains("AGGREGATION_TEMPORALITY"));
    }

    #[test]
    fn microseconds_become_decimal_seconds_without_integer_clamping() {
        for (usec, seconds) in [
            (0, "0.000000"),
            (1, "0.000001"),
            (999_999, "0.999999"),
            (1_000_000, "1.000000"),
            (u64::MAX, "18446744073709.551615"),
        ] {
            let mut value = sample();
            value.cpu_usec = Ok(usec);
            assert!(encode(&[value]).contains(&format!("\"asDouble\":{seconds}")));
        }
    }

    #[test]
    fn unavailable_values_are_omitted_with_separate_typed_diagnostics() {
        let value = Snapshot {
            cpu_usec: Err(Failure::Missing),
            memory_bytes: Err(Failure::Io(2)),
            threads: Err(Failure::Invalid),
            read_bytes: Err(Failure::Overflow),
            write_bytes: Ok(34),
            ..sample()
        };
        let text = encode(&[value]);
        assert_eq!(text.matches("\"aggregationTemporality\":2").count(), 1);
        assert!(text.contains("\"shards.cgroup.collection.error.count\",\"unit\":\"{error}\",\"gauge\":{\"dataPoints\":[{\"timeUnixNano\":\"18446744073709551615\",\"asInt\":\"4\"}"));
        for cause in ["missing", "io", "invalid", "overflow"] {
            assert!(text.contains(&format!(
                "\"error.type\",\"value\":{{\"stringValue\":\"{cause}\"}}"
            )));
        }
        assert!(text.contains("\"shards.error.errno\",\"value\":{\"intValue\":\"2\"}"));
        assert!(!text.contains("\"flags\""));
        assert!(!text.contains("\"asInt\":\"0\""));
    }

    #[test]
    fn int64_boundary_is_preserved_and_overflow_is_not_a_measurement() {
        let mut value = sample();
        value.memory_bytes = Ok(i64::MAX as u64);
        assert!(encode(&[value]).contains("\"asInt\":\"9223372036854775807\""));
        value.memory_bytes = Ok(i64::MAX as u64 + 1);
        let text = encode(&[value]);
        assert!(!text.contains("\"name\":\"shards.cgroup.memory.usage\""));
        assert!(text.contains("\"error.type\",\"value\":{\"stringValue\":\"overflow\"}"));
        assert!(!text.contains("9223372036854775808"));
        value.memory_bytes = Ok(u64::MAX);
        assert!(!encode(&[value]).contains("\"name\":\"shards.cgroup.memory.usage\""));
    }

    #[test]
    fn every_collection_has_a_current_error_count_even_after_recovery() {
        let mut value = sample();
        value.cpu_usec = Err(Failure::Missing);
        let failed = encode(&[value]);
        assert!(failed.contains("\"name\":\"shards.cgroup.collection.error\""));
        let healthy = encode(&[sample()]);
        assert!(!healthy.contains("\"name\":\"shards.cgroup.collection.error\""));
        assert!(healthy.contains("\"shards.cgroup.collection.error.count\",\"unit\":\"{error}\",\"gauge\":{\"dataPoints\":[{\"timeUnixNano\":\"18446744073709551615\",\"asInt\":\"0\"}"));
    }

    #[test]
    fn an_invalid_timestamp_is_rejected_before_any_output() {
        let mut invalid = sample();
        invalid.time_unix_nano = 0;
        let mut bytes = Vec::new();
        let error = write(&mut bytes, "run", "vm", 0, &[sample(), invalid]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bytes.is_empty());
    }

    #[test]
    fn empty_collection_is_one_empty_request() {
        assert_eq!(encode(&[]), "{\"resourceMetrics\":[]}\n");
    }

    #[test]
    fn utf8_is_preserved_and_json_controls_cannot_escape_strings() {
        let mut text = String::from("é—🦀\"\\/");
        for code in 0..=0x1f {
            text.push(char::from_u32(code).unwrap());
        }
        let mut bytes = Vec::new();
        string(&mut bytes, &text).unwrap();
        let encoded = String::from_utf8(bytes).unwrap();
        assert!(encoded.starts_with("\"é—🦀\\\"\\\\/"));
        for code in 0..=0x1f {
            assert!(encoded.contains(&format!("\\u{code:04x}")));
        }
        assert!(encoded.ends_with('"'));
        assert!(!encoded.as_bytes().iter().any(|b| *b < 0x20));
    }

    struct Ending {
        bytes: Vec<u8>,
        after: usize,
    }

    impl Write for Ending {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let room = self.after.saturating_sub(self.bytes.len());
            if room == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let count = room.min(bytes.len()).min(1);
            self.bytes.extend_from_slice(bytes.get(..count).unwrap());
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_writes_and_sink_failure_are_returned() {
        let complete = encode(&[sample()]);
        for after in [0, 1, complete.len() / 2, complete.len() - 1] {
            let mut ending = Ending {
                bytes: Vec::new(),
                after,
            };
            let error = write(&mut ending, "run", "vm", u64::MAX, &[sample()]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            assert_eq!(ending.bytes, complete.as_bytes().get(..after).unwrap());
        }
        let mut whole = Ending {
            bytes: Vec::new(),
            after: complete.len(),
        };
        write(&mut whole, "run", "vm", u64::MAX, &[sample()]).unwrap();
        assert_eq!(whole.bytes, complete.as_bytes());
    }
}
