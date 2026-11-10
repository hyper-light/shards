"""Syntax and narrow wire-contract check; not a Collector conformance result."""

import json
import sys

raw = sys.stdin.buffer.read()
request = json.loads(raw)
assert raw.endswith(b"\n") and raw.count(b"\n") == 1
assert set(request) == {"resourceMetrics"}
resources = request["resourceMetrics"]
assert len(resources) == 3
for resource in resources:
    attributes = {a["key"]: a["value"] for a in resource["resource"]["attributes"]}
    assert attributes["shards.microvm.id"] == {
        "stringValue": 'é—🦀"\\/\x00\x08\t\n\r\x1b\x1f'
    }
    assert attributes["shards.microvm.generation"] == {
        "stringValue": str(2**64 - 1)
    }
    scope = resource["scopeMetrics"]
    assert len(scope) == 1 and scope[0]["scope"] == {"name": "shards.init"}
    metrics = scope[0]["metrics"]
    names = [m["name"] for m in metrics]
    assert len(names) == len(set(names))
    for metric in metrics:
        aggregation = metric.get("sum", metric.get("gauge"))
        assert aggregation is not None
        if "sum" in metric:
            assert aggregation["aggregationTemporality"] == 2
            assert isinstance(aggregation["isMonotonic"], bool)
        assert aggregation["dataPoints"]
        for point in aggregation["dataPoints"]:
            assert point["timeUnixNano"] == "1794335411506000001"
            assert "startTimeUnixNano" not in point
            assert "flags" not in point
            assert ("asInt" in point) != ("asDouble" in point)
            if "asInt" in point:
                assert isinstance(point["asInt"], str)
                assert -(2**63) <= int(point["asInt"]) < 2**63
            attrs = point.get("attributes", [])
            assert len(attrs) == len({a["key"] for a in attrs})
    by_name = {m["name"]: m for m in metrics}
    failures = by_name["shards.cgroup.collection.error.count"]["gauge"]["dataPoints"]
    own = attributes["shards.cgroup.scope"]["stringValue"]
    if own == "run":
        assert failures[0]["asInt"] == "0"
        assert by_name["shards.cgroup.cpu.time"]["sum"]["dataPoints"][0]["asDouble"] == 1.000001
        assert by_name["shards.cgroup.memory.usage"]["sum"]["dataPoints"][0]["asInt"] == "0"
    elif own == "workload":
        assert failures[0]["asInt"] == "4"
        assert "shards.cgroup.cpu.time" not in by_name
        assert "shards.cgroup.memory.usage" not in by_name
        assert "shards.cgroup.task.count" not in by_name
        io = by_name["shards.cgroup.disk.io"]["sum"]["dataPoints"]
        assert len(io) == 1 and io[0]["asInt"] == "34"
        errors = by_name["shards.cgroup.collection.error"]["gauge"]["dataPoints"]
        assert len(errors) == 4
        assert all(p["asInt"] == "1" for p in errors)
    elif own == "member":
        assert attributes["shards.member.id"] == {"intValue": str(2**32 - 1)}
        assert failures[0]["asInt"] == "1"
        assert "shards.cgroup.memory.usage" not in by_name
        assert by_name["shards.cgroup.task.count"]["sum"]["dataPoints"][0]["asInt"] == str(2**63 - 1)
    else:
        raise AssertionError(own)
print(json.dumps({"resources": len(resources), "json_and_selected_contract": "passed", "collector_conformance": "not_run"}))
