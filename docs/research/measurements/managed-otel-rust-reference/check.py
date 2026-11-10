#!/usr/bin/env python3
"""Check the three actual payloads; no timings, live counters or collectors."""

import hashlib
import json
import math
import pathlib
import sys

MODES = ("native-stream", "official-compact-buffer", "official-pretty-buffer")


def fields(value, required, defaults=None):
    defaults = defaults or {}
    assert isinstance(value, dict), value
    assert set(required) <= value.keys(), (required, value)
    assert value.keys() <= set(required) | defaults.keys(), value
    for key, default in defaults.items():
        assert value.get(key, default) == default, (key, value)


def integer(value, signed=True):
    assert isinstance(value, str), value
    result = int(value)
    assert str(result) == value, value
    low, high = (-(2**63), 2**63 - 1) if signed else (0, 2**64 - 1)
    assert low <= result <= high, value
    return result


def attributes(items):
    result = {}
    for attr in items:
        fields(attr, ("key", "value"), {"keyStrindex": 0})
        key, value = attr["key"], attr["value"]
        assert isinstance(key, str) and key not in result, key
        assert isinstance(value, dict) and len(value) == 1, value
        kind, data = next(iter(value.items()))
        if kind == "stringValue":
            assert isinstance(data, str), value
        else:
            assert kind == "intValue", value
            data = integer(data)
        result[key] = [kind, data]
    return result


def point(value):
    number = set(value) & {"asInt", "asDouble"}
    assert len(number) == 1, value
    kind = next(iter(number))
    fields(value, ("timeUnixNano", kind), {
        "attributes": value.get("attributes", []),
        "startTimeUnixNano": "0", "flags": 0, "exemplars": [],
    })
    timestamp = integer(value["timeUnixNano"], signed=False)
    assert timestamp != 0, value
    if kind == "asInt":
        number = integer(value[kind])
    else:
        number = value[kind]
        assert isinstance(number, (int, float)) and math.isfinite(number), value
        number = float(number).hex()
    return {"timestamp": timestamp, "attributes": attributes(value.get("attributes", [])),
            "value": [kind, number], "start": None, "flags": 0}


def metric(value):
    kinds = set(value) & {"sum", "gauge"}
    assert len(kinds) == 1, value
    kind = next(iter(kinds))
    fields(value, ("name", "unit", kind), {"description": "", "metadata": []})
    data = value[kind]
    required = ("dataPoints", "aggregationTemporality", "isMonotonic") if kind == "sum" else ("dataPoints",)
    fields(data, required)
    normalized = {"name": value["name"], "unit": value["unit"], "kind": kind,
                  "points": [point(item) for item in data["dataPoints"]]}
    assert normalized["points"], value
    if kind == "sum":
        assert data["aggregationTemporality"] == 2, data
        assert isinstance(data["isMonotonic"], bool), data
        normalized.update(temporality=2, monotonic=data["isMonotonic"])
    return normalized


def receipt(payload):
    fields(payload, ("resourceMetrics",))
    result = []
    for resource in payload["resourceMetrics"]:
        fields(resource, ("resource", "scopeMetrics"), {"schemaUrl": ""})
        fields(resource["resource"], ("attributes",), {"droppedAttributesCount": 0, "entityRefs": []})
        scopes = []
        for scope in resource["scopeMetrics"]:
            fields(scope, ("scope", "metrics"), {"schemaUrl": ""})
            fields(scope["scope"], ("name",), {"version": "", "attributes": [], "droppedAttributesCount": 0})
            assert scope["scope"]["name"] == "shards.init", scope
            scopes.append({"scope": "shards.init", "metrics": [metric(item) for item in scope["metrics"]]})
        result.append({"resource": attributes(resource["resource"]["attributes"]), "scopes": scopes})
    return result


def check(directory):
    results, receipts = {}, []
    for mode in MODES:
        raw = (directory / (mode + ".json")).read_bytes()
        normalized = receipt(json.loads(raw))
        receipt_bytes = json.dumps(normalized, sort_keys=True, ensure_ascii=True, separators=(",", ":")).encode() + b"\n"
        (directory / (mode + ".receipt.json")).write_bytes(receipt_bytes)
        metrics = [metric for resource in normalized for scope in resource["scopes"] for metric in scope["metrics"]]
        points = [point for metric in metrics for point in metric["points"]]
        assert (len(normalized), len(metrics), len(points)) == (3, 18, 41)
        scope_names = [resource["resource"]["shards.cgroup.scope"] for resource in normalized]
        assert scope_names == [["stringValue", name] for name in ("run", "workload", "member")]
        for resource in normalized:
            assert resource["resource"]["shards.microvm.restore.generation"] == ["stringValue", str(2**64 - 1)]
        assert normalized[-1]["resource"]["shards.member.id"] == ["intValue", 2**32 - 1]
        errors = [metric["points"][0]["value"] for metric in metrics if metric["name"] == "shards.cgroup.collection.error.count"]
        assert errors == [["asInt", number] for number in (0, 6, 2)], errors
        starts = raw.count(b'"startTimeUnixNano"')
        assert starts == (0 if mode == "native-stream" else 41), starts
        receipts.append(normalized)
        results[mode] = {"payload_bytes": len(raw), "payload_sha256": hashlib.sha256(raw).hexdigest(),
                         "receipt_sha256": hashlib.sha256(receipt_bytes).hexdigest(),
                         "resources": 3, "metrics": 18, "points": 41, "explicit_unknown_start_fields": starts}
    assert all(item == receipts[0] for item in receipts)
    return {"semantic_receipts_equal": True, "scope": "fixed shared Snapshot fixture; encoding only", "modes": results}


if __name__ == "__main__":
    print(json.dumps(check(pathlib.Path(sys.argv[1])), indent=2, sort_keys=True))
