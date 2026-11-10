use std::io;

use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum, metric,
    number_data_point,
};
use opentelemetry_proto::tonic::resource::v1::Resource;

use crate::{Failure, Scope, Snapshot};
use native_codec::Limit;

type Value = number_data_point::Value;

pub(super) fn request(
    run: &str,
    microvm: &str,
    generation: u64,
    samples: &[Snapshot],
) -> io::Result<ExportMetricsServiceRequest> {
    if samples.iter().any(|sample| sample.time_unix_nano == 0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(ExportMetricsServiceRequest {
        resource_metrics: samples
            .iter()
            .map(|sample| resource(run, microvm, generation, sample))
            .collect::<io::Result<Vec<_>>>()?,
    })
}

fn string(key: &str, value: &str) -> KeyValue {
    attribute(key, any_value::Value::StringValue(value.into()))
}

fn integer(key: &str, value: i64) -> KeyValue {
    attribute(key, any_value::Value::IntValue(value))
}

fn attribute(key: &str, value: any_value::Value) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}

fn point(timestamp: u64, value: Value, attributes: Vec<KeyValue>) -> NumberDataPoint {
    NumberDataPoint {
        time_unix_nano: timestamp,
        value: Some(value),
        attributes,
        // Unknown start stays the generated proto default. Its unchanged serde
        // serializer emits "0", semantically unknown like native omission.
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Instrument {
    Cpu,
    Memory,
    Tasks,
    Io,
    MemoryLimit,
    TaskLimit,
}

impl Instrument {
    fn fields(self) -> (&'static str, &'static str, bool) {
        match self {
            Self::Cpu => ("shards.cgroup.cpu.time", "s", true),
            Self::Memory => ("shards.cgroup.memory.usage", "By", false),
            Self::Tasks => ("shards.cgroup.task.count", "{task}", false),
            Self::Io => ("shards.cgroup.disk.io", "By", true),
            Self::MemoryLimit => ("shards.cgroup.memory.limit", "By", false),
            Self::TaskLimit => ("shards.cgroup.task.limit", "{task}", false),
        }
    }
}

struct Observation {
    instrument: Instrument,
    direction: Option<&'static str>,
    value: Result<Option<Value>, Failure>,
}

fn observations(sample: &Snapshot) -> io::Result<[Observation; 7]> {
    let cpu = match sample.cpu_usec {
        Ok(usec) => {
            // Parse the same exact decimal rational as the native wire. Casting
            // u64 to f64 before division could round before the SI conversion.
            const PER_SECOND: u64 = 1_000_000;
            let decimal = format!("{}.{:06}", usec / PER_SECOND, usec % PER_SECOND);
            Ok(Value::AsDouble(decimal.parse().map_err(io::Error::other)?))
        }
        Err(cause) => Err(cause),
    };
    let signed = |instrument, direction, value: Result<u64, Failure>| Observation {
        instrument,
        direction,
        value: value.and_then(|number| {
            i64::try_from(number)
                .map(|n| Some(Value::AsInt(n)))
                .map_err(|_| Failure::Overflow)
        }),
    };
    let limit = |instrument, value: Result<Limit, Failure>| Observation {
        instrument,
        direction: None,
        value: value.and_then(|value| match value {
            Limit::Finite(n) => i64::try_from(n)
                .map(|n| Some(Value::AsInt(n)))
                .map_err(|_| Failure::Overflow),
            Limit::Unbounded => Ok(None),
        }),
    };
    Ok([
        Observation {
            instrument: Instrument::Cpu,
            direction: None,
            value: cpu.map(Some),
        },
        signed(Instrument::Memory, None, sample.memory_bytes),
        signed(Instrument::Tasks, None, sample.threads),
        signed(Instrument::Io, Some("read"), sample.read_bytes),
        signed(Instrument::Io, Some("write"), sample.write_bytes),
        limit(Instrument::MemoryLimit, sample.memory_limit),
        limit(Instrument::TaskLimit, sample.task_limit),
    ])
}

fn directions(direction: Option<&str>) -> Vec<KeyValue> {
    direction
        .map(|value| string("disk.io.direction", value))
        .into_iter()
        .collect()
}

fn gauge(name: &str, unit: &str, points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.into(),
        unit: unit.into(),
        data: Some(metric::Data::Gauge(Gauge { data_points: points })),
        ..Default::default()
    }
}

fn metrics(sample: &Snapshot) -> io::Result<Vec<Metric>> {
    let observations = observations(sample)?;
    let mut metrics = Vec::new();
    for instrument in [
        Instrument::Cpu,
        Instrument::Memory,
        Instrument::Tasks,
        Instrument::Io,
        Instrument::MemoryLimit,
        Instrument::TaskLimit,
    ] {
        let points: Vec<_> = observations
            .iter()
            .filter(|observation| observation.instrument == instrument)
            .filter_map(|observation| {
                observation
                    .value
                    .ok()
                    .flatten()
                    .map(|value| point(sample.time_unix_nano, value, directions(observation.direction)))
            })
            .collect();
        if !points.is_empty() {
            let (name, unit, monotonic) = instrument.fields();
            if matches!(instrument, Instrument::MemoryLimit | Instrument::TaskLimit) {
                metrics.push(gauge(name, unit, points));
                continue;
            }
            metrics.push(Metric {
                name: name.into(),
                unit: unit.into(),
                data: Some(metric::Data::Sum(Sum {
                    data_points: points,
                    aggregation_temporality: AggregationTemporality::Cumulative.into(),
                    is_monotonic: monotonic,
                })),
                ..Default::default()
            });
        }
    }
    let mut states = Vec::new();
    for (instrument, value) in [
        (Instrument::MemoryLimit, sample.memory_limit),
        (Instrument::TaskLimit, sample.task_limit),
    ] {
        let observed = match value {
            Ok(Limit::Finite(_)) => "finite",
            Ok(Limit::Unbounded) => "unbounded",
            Err(_) => "unavailable",
        };
        for state in ["finite", "unbounded", "unavailable"] {
            states.push(point(
                sample.time_unix_nano,
                Value::AsInt(i64::from(u8::from(state == observed))),
                vec![
                    string("shards.cgroup.metric", instrument.fields().0),
                    string("shards.cgroup.limit.state", state),
                ],
            ));
        }
    }
    metrics.push(gauge("shards.cgroup.limit.state", "1", states));
    let failures = observations.iter().filter(|value| value.value.is_err()).count();
    metrics.push(gauge(
        "shards.cgroup.collection.error.count",
        "{error}",
        vec![point(
            sample.time_unix_nano,
            Value::AsInt(i64::try_from(failures).map_err(io::Error::other)?),
            Vec::new(),
        )],
    ));
    let errors: Vec<_> = observations
        .iter()
        .filter_map(|observation| {
            let cause = observation.value.err()?;
            let mut attrs = vec![string("shards.cgroup.metric", observation.instrument.fields().0)];
            attrs.extend(directions(observation.direction));
            attrs.push(string(
                "error.type",
                match cause {
                    Failure::Io(_) => "io",
                    Failure::Missing => "missing",
                    Failure::Invalid => "invalid",
                    Failure::Overflow => "overflow",
                },
            ));
            if let Failure::Io(errno) = cause {
                attrs.push(integer("shards.error.errno", i64::from(errno)));
            }
            Some(point(sample.time_unix_nano, Value::AsInt(1), attrs))
        })
        .collect();
    if !errors.is_empty() {
        metrics.push(gauge("shards.cgroup.collection.error", "{error}", errors));
    }
    Ok(metrics)
}

fn resource(run: &str, microvm: &str, generation: u64, sample: &Snapshot) -> io::Result<ResourceMetrics> {
    let mut attributes = vec![
        string("shards.run.id", run),
        string("shards.microvm.id", microvm),
        string("shards.microvm.restore.generation", &generation.to_string()),
        string(
            "shards.cgroup.scope",
            match sample.scope {
                Scope::Run => "run",
                Scope::Workload => "workload",
                Scope::Member(_) => "member",
            },
        ),
    ];
    if let Scope::Member(member) = sample.scope {
        attributes.push(integer("shards.member.id", i64::from(member)));
    }
    Ok(ResourceMetrics {
        resource: Some(Resource {
            attributes,
            ..Default::default()
        }),
        scope_metrics: vec![ScopeMetrics {
            scope: Some(InstrumentationScope {
                name: "shards.init".into(),
                ..Default::default()
            }),
            metrics: metrics(sample)?,
            ..Default::default()
        }],
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn complete_official_requests_match_native_for_the_fixed_limit_boundary_corpus() {
        for value in [
            Ok(Limit::Finite(0)),
            Ok(Limit::Finite(i64::MAX as u64)),
            Ok(Limit::Finite(i64::MAX as u64 + 1)),
            Ok(Limit::Finite(u64::MAX)),
            Ok(Limit::Unbounded),
            Err(Failure::Missing),
            Err(Failure::Invalid),
            Err(Failure::Overflow),
            Err(Failure::Io(2)),
        ] {
            for scope in [Scope::Run, Scope::Workload, Scope::Member(u32::MAX)] {
                let sample = Snapshot {
                    scope,
                    time_unix_nano: u64::MAX,
                    cpu_usec: Ok(1_000_001),
                    memory_bytes: Ok(0),
                    threads: Ok(4),
                    read_bytes: Ok(12),
                    write_bytes: Ok(34),
                    memory_limit: value,
                    task_limit: Ok(Limit::Unbounded),
                };
                for sample in [
                    sample,
                    Snapshot {
                        memory_limit: Ok(Limit::Unbounded),
                        task_limit: value,
                        ..sample
                    },
                ] {
                    let mut bytes = Vec::new();
                    native_codec::write(&mut bytes, "run", "vm", u64::MAX, &[sample]).unwrap();
                    let native: ExportMetricsServiceRequest = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(native, request("run", "vm", u64::MAX, &[sample]).unwrap());
                }
            }
        }
    }
}
