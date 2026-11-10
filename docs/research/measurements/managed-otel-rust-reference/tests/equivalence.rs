#![allow(clippy::unwrap_used, clippy::panic)]

use std::process::Command;

use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;

#[test]
fn the_shared_fixture_decodes_identically_through_official_types() {
    let mut decoded = Vec::new();
    for mode in [
        "native-stream",
        "official-compact-buffer",
        "official-pretty-buffer",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_shards-managed-otel-rust-reference"))
            .arg(mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode}: {:?}", output.stderr);
        assert!(output.stderr.is_empty(), "{mode}: {:?}", output.stderr);
        decoded
            .push(serde_json::from_slice::<ExportMetricsServiceRequest>(&output.stdout).unwrap());
    }
    for request in &decoded {
        assert_eq!(Some(request), decoded.first());
        assert_eq!(request.resource_metrics.len(), 3);
        for resource in &request.resource_metrics {
            for scope in &resource.scope_metrics {
                for metric in &scope.metrics {
                    use opentelemetry_proto::tonic::metrics::v1::metric::Data;
                    let points = match &metric.data {
                        Some(Data::Sum(sum)) => &sum.data_points,
                        Some(Data::Gauge(gauge)) => &gauge.data_points,
                        other => panic!("unexpected fixture data: {other:?}"),
                    };
                    for point in points {
                        assert_eq!(point.start_time_unix_nano, 0);
                        assert_ne!(point.time_unix_nano, 0);
                    }
                }
            }
        }
    }
}
