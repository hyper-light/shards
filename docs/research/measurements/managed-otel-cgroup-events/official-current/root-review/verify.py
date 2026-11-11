from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import ast
import math
import subprocess
import sys
import tarfile


def identity(data):
    return {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def payloads(source):
    result = {}
    with tarfile.open(fileobj=source) as archive:
        for member in archive:
            path = PurePosixPath(member.name)
            assert member.isfile() and not path.is_absolute() and ".." not in path.parts
            assert member.name not in result
            result[member.name] = archive.extractfile(member).read()
    return result


base = Path(sys.argv[1])
repo = Path(sys.argv[2])
files = payloads(io.BytesIO((base / "owned-files.tar.xz").read_bytes()))
manifest = json.loads((base / "MANIFEST.json").read_bytes())
assert {name: identity(data) for name, data in files.items()} == manifest
prefix = "docs/research/measurements/managed-otel-cgroup-events/official-current/"
assert len(files) == 261 and all(name.startswith(prefix) for name in files)
archives = {}
for relative, count in [("v1/prepare", 44), ("v2/prepare", 44), ("v2/consume", 121)]:
    name = prefix + "records/" + relative + "-raw.tar.gz"
    archive_bytes = files[name]
    inner = payloads(io.BytesIO(archive_bytes))
    original_manifest = json.loads(inner.pop("members.json"))
    assert len(inner) == count
    assert {name: identity(data) for name, data in inner.items()} == original_manifest
    for name, data in inner.items():
        assert files[prefix + "records/" + relative + "/" + name] == data
    archives[relative] = {"payloads": count, "manifest": 1, **identity(archive_bytes)}

result = json.loads(files[prefix + "records/v2/consume/result.json"])
assert result["ROOT_source"] == "a8ad1ce8718bb08d8acaffce7c85e25ce49bce49"
assert result["outcome"] == 0 and result["retry"] is False
assert result["runtime_attempts"] == len(result["commands"]) == 19
assert result["source_before"] == result["source_after"]
assert result["fd_before"] == result["fd_after"]
assert len(result["cases"]) == 9
driver = ast.parse(files[prefix + "run.py"])
definitions = [node for node in driver.body if isinstance(node, ast.FunctionDef) and node.name in {"attributes", "check_receipt"}]
assert len(definitions) == 2
comparison = {"math": math}
exec(compile(ast.Module(body=definitions, type_ignores=[]), "reviewed-offline-comparison", "exec"), comparison)
for case in result["cases"]:
    assert case["http"] == 200
    assert case["child_reaped"] and case["collector_counts_equal"]
    assert case["all_official_decoded_fields_equal_source"]
    data = files[prefix + "records/v2/prepare/copied-inputs/" + case["input"]]
    assert identity(data) == case["input_identity"]
    label = case["input"].removesuffix(".json").replace("/", "-")
    raw = prefix + "records/v2/consume/"
    decoded = json.loads(files[raw + label + "-decode.stdout"])
    counts = comparison["check_receipt"](json.loads(data), decoded)
    assert all(counts[key] == case[key] for key in counts)
    http = json.loads(files[raw + label + "/http/result.json"])
    assert http["passed"] and http["http_passed"] and http["status"] == 200
    assert http["input_sha256"] == identity(data)["sha256"]
    assert http["collector_exit"] == 0 and http["log_reader_joined"]
    summaries = [json.loads(line) for line in files[raw + label + "/http/collector.log"].splitlines()]
    summaries = [row for row in summaries if row.get("msg") == "Metrics"]
    assert len(summaries) == 1
    assert tuple(summaries[0][key] for key in ["resource metrics", "metrics", "data points"]) == tuple(counts[key] for key in ["resources", "metrics", "points"])
    observation = json.loads(files[raw + label + "/pidfd.json"])
    assert observation["fd_before"] == observation["fd_after"]
    assert len(observation["children"]) == 1
    child = observation["children"][0]
    assert child["pidfd_closed_before_harness_fd_check"]
    assert child["waits"][-1] == {"returncode": 0, "pidfd_exit_ready": True, "direct_child_reaped": True}
for command in result["commands"]:
    assert command["exit_code"] == 0
    assert command["direct_child_reaped"] and command["pidfd_exit_ready"]
    assert command["fd_before"] == command["fd_after"]

assert result["artifacts_after"]["otelcol"]["sha256"] == "14e254a6a0880d9b6f94487658e49fc9f3b0e14c62e82253be78da08e8c1ddd8"
source_files = {
    "collector-http.py": "docs/research/measurements/managed-otel/collector-http.py",
    **{f"oracle/{name}": f"docs/research/measurements/managed-otel-oracle/{name}"
       for name in ["go.mod", "go.sum", "main.go", "main_test.go", "upstream.json"]},
}
for name, path in source_files.items():
    original = subprocess.check_output(["git", "show", "0a6d91b3de0f10a8397868aa3794adab1e09afdc:" + path], cwd=repo)
    assert identity(original) == result["source_before"][name]

review = {
    "coordinator": "01a148884a4770fdaf0111f3f792a248",
    "policy_revision": "7a897def2ac3bb041da584d1d6854fa066ad7fc2",
    "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
    "snapshot_files_verified": len(files),
    "snapshot_archive": identity((base / "owned-files.tar.xz").read_bytes()),
    "original_archives": archives,
    "official_decodes_and_posts": len(result["cases"]),
    "direct_command_pidfd_reaping_and_fd_return": len(result["commands"]),
    "official_consumer_sources_equal_committed": len(source_files),
    "runtime_replay": False,
    "limits": ["Component interoperability only", "Production exporter delivery, kernel backing, continuity, full management and costs remain open"],
}
print(json.dumps(review, indent=2))
