#!/usr/bin/env python3
"""Re-read pinned primary sources and check only the fields this fixture uses."""

import hashlib
import json
import re
import urllib.request

RUST = "372ebbc74d4dba71bb9820732f57f7316b4e7a1e"
EMBEDDED = "ca839c51f706f5d53bfb46f06c3e90c3af3a52c6"
WIRE = "b3f75588eb23c5fca62264edd05d382de49beb1a"
SCHEMAS = ("collector/metrics/v1/metrics_service.proto", "metrics/v1/metrics.proto",
           "common/v1/common.proto", "resource/v1/resource.proto")
USED = {
    "ExportMetricsServiceRequest": ("resource_metrics",),
    "ResourceMetrics": ("resource", "scope_metrics", "schema_url"),
    "ScopeMetrics": ("scope", "metrics", "schema_url"),
    "Metric": ("name", "description", "unit", "gauge", "sum", "metadata"),
    "Sum": ("data_points", "aggregation_temporality", "is_monotonic"),
    "Gauge": ("data_points",),
    "NumberDataPoint": ("attributes", "start_time_unix_nano", "time_unix_nano",
                        "exemplars", "flags", "as_double", "as_int"),
    "KeyValue": ("key", "value"), "AnyValue": ("string_value", "int_value"),
    "InstrumentationScope": ("name", "version", "attributes", "dropped_attributes_count"),
    "Resource": ("attributes", "dropped_attributes_count", "entity_refs"),
}


def fetch(url, record):
    with urllib.request.urlopen(url, timeout=30) as response:
        data = response.read()
    record.append({"url": url, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
    return data


def used_fields(data):
    text = re.sub(r"/\*.*?\*/|//[^\n]*", "", data.decode(), flags=re.S)
    result = {}
    for message, names in USED.items():
        match = re.search(r"\bmessage\s+" + message + r"\s*\{", text)
        if match is None:
            continue
        at = end = match.end()
        depth = 1
        while depth:
            character = text[end]
            depth += (character == "{") - (character == "}")
            end += 1
        body = text[at:end - 1]
        for name in names:
            field = re.search(r"\b(?:(repeated|optional)\s+)?([.\w]+)\s+" + name + r"\s*=\s*(\d+)", body)
            if field is None:
                raise ValueError(f"missing {message}.{name}")
            result[message + "." + name] = {"cardinality": field[1] or "singular",
                                           "type": field[2], "tag": int(field[3])}
    return result


def main():
    sources, fields = [], {}
    for revision in (EMBEDDED, WIRE):
        collected = {}
        for path in SCHEMAS:
            url = f"https://raw.githubusercontent.com/open-telemetry/opentelemetry-proto/{revision}/opentelemetry/proto/{path}"
            data = fetch(url, sources)
            collected.update(used_fields(data))
            if path == "metrics/v1/metrics.proto":
                assert re.search(rb"AGGREGATION_TEMPORALITY_CUMULATIVE\s*=\s*2\s*;", data)
        fields[revision] = collected
    assert fields[EMBEDDED] == fields[WIRE]
    assert len(fields[EMBEDDED]) == sum(map(len, USED.values()))
    paths = ["opentelemetry-proto/Cargo.toml", "opentelemetry-proto/src/proto.rs",
             "opentelemetry-proto/tests/grpc_build.rs",
             "opentelemetry-sdk/src/metrics/data/mod.rs",
             "opentelemetry-otlp/src/exporter/http/mod.rs"]
    paths += ["opentelemetry-proto/src/proto/tonic/opentelemetry.proto." + name + ".v1.rs"
              for name in ("collector.metrics", "metrics", "common", "resource")]
    for path in paths:
        fetch(f"https://raw.githubusercontent.com/open-telemetry/opentelemetry-rust/{RUST}/{path}", sources)
    with urllib.request.urlopen(f"https://api.github.com/repos/open-telemetry/opentelemetry-rust/git/trees/{RUST}?recursive=1", timeout=30) as response:
        tree = json.load(response)
    gitlink = next(item for item in tree["tree"] if item["path"] == "opentelemetry-proto/src/proto/opentelemetry-proto")
    assert gitlink["type"] == "commit" and gitlink["sha"] == EMBEDDED
    return {"rust_release": "0.33.1", "rust_revision": RUST, "embedded_proto_revision": EMBEDDED,
            "comparison_proto_release": "v1.11.1", "comparison_proto_revision": WIRE,
            "used_fields_equal": True, "used_fields": fields[EMBEDDED], "sources": sources,
            "embedded_gitlink": gitlink,
            "limits": "Used field names/tags/types/cardinalities and cumulative enum only; not whole-schema or SDK-pipeline equivalence."}


if __name__ == "__main__":
    print(json.dumps(main(), indent=2, sort_keys=True))
