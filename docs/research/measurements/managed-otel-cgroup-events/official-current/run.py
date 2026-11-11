#!/usr/bin/env python3
"""One finite official consumer batch over nine frozen authorized artifacts."""
import hashlib
import io
import json
import math
import os
from pathlib import Path, PurePosixPath
import platform
import re
import select
import shutil
import stat
import subprocess
import sys
import tarfile
import time
import urllib.request


SOURCE = "a8ad1ce8718bb08d8acaffce7c85e25ce49bce49"
ROOT = Path("/home/adalundhe/root-managed-events-a8ad1ce8718b-v1")
INPUTS = ["metrics/first.json", "metrics/second.json", "metrics/finite.json", "metrics/unbounded.json",
          "joiner/provider.json", "joiner/joiner.json", "events/before.json", "events/finite.json", "events/after-refusal.json"]
COLLECTOR = "14e254a6a0880d9b6f94487658e49fc9f3b0e14c62e82253be78da08e8c1ddd8"
ASSET = "78570dd4bd6570bc41eb327869ef913bbf91b87dd8170ebebbf43bec71f6370d"
URL = "https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.165.0/otelcol_0.165.0_linux_amd64.tar.gz"


def identity(path):
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"bytes": path.stat().st_size, "sha256": digest}


def read_root(name):
    path = ROOT / name
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW), "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise RuntimeError("authorized input is not a regular file")
        data = stream.read(before.st_size + 1)
        after = os.fstat(stream.fileno())
    if len(data) != before.st_size or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
        raise RuntimeError("authorized frozen artifact changed during its one read")
    return data


def attributes(source, decoded):
    expected = []
    for item in source:
        value = item["value"]
        if "stringValue" in value:
            kind, scalar = "Str", value["stringValue"]
        elif "intValue" in value:
            kind, scalar = "Int", str(int(value["intValue"]))
        elif "boolValue" in value:
            kind, scalar = "Bool", value["boolValue"]
        else:
            raise RuntimeError("unexpected attribute kind in these fixed Shards artifacts")
        expected.append({"key": item["key"], "kind": kind, "value": scalar})
    assert sorted(expected, key=lambda a: a["key"]) == decoded


def check_receipt(source, receipt):
    assert receipt["decoder"] == "go.opentelemetry.io/collector/pdata@v1.71.0"
    groups = source["resourceMetrics"]
    assert len(groups) == len(receipt["resources"])
    metrics, points = 0, 0
    for group, resource in zip(groups, receipt["resources"]):
        assert resource["schemaUrl"] == group.get("schemaUrl", "")
        attributes(group.get("resource", {}).get("attributes", []), resource["attributes"])
        assert len(group["scopeMetrics"]) == len(resource["scopes"])
        for raw_scope, scope in zip(group["scopeMetrics"], resource["scopes"]):
            instrument = raw_scope.get("scope", {})
            assert (scope["name"], scope["version"], scope["schemaUrl"]) == (instrument.get("name", ""), instrument.get("version", ""), raw_scope.get("schemaUrl", ""))
            attributes(instrument.get("attributes", []), scope["attributes"])
            assert len(raw_scope["metrics"]) == len(scope["metrics"])
            for raw, metric in zip(raw_scope["metrics"], scope["metrics"]):
                metrics += 1
                kind = "sum" if "sum" in raw else "gauge"
                assert (metric["name"], metric["unit"], metric["type"]) == (raw["name"], raw.get("unit", ""), kind.title())
                body = raw[kind]
                if kind == "sum":
                    assert metric["temporality"] == {1: "Delta", 2: "Cumulative"}[body["aggregationTemporality"]]
                    assert metric["monotonic"] == body.get("isMonotonic", False)
                assert len(body["dataPoints"]) == len(metric["points"])
                for raw_point, point in zip(body["dataPoints"], metric["points"]):
                    points += 1
                    assert (point["startTimeUnixNano"], point["timeUnixNano"], point["flags"]) == tuple(str(int(raw_point.get(k, "0"))) for k in ("startTimeUnixNano", "timeUnixNano", "flags"))
                    attributes(raw_point.get("attributes", []), point["attributes"])
                    if "asInt" in raw_point:
                        assert point["kind"] == "Int" and point["value"] == str(int(raw_point["asInt"]))
                    else:
                        expected = float(raw_point["asDouble"])
                        assert point["kind"] == "Double" and math.isfinite(expected) and float(point["value"]) == expected
    return {"resources": len(groups), "metrics": metrics, "points": points,
            "all_official_decoded_fields_equal_source": True}


def main():
    phase = sys.argv[1]
    assert phase in ("prepare", "consume") and len(sys.argv) == 2
    base = Path.cwd().resolve()
    assert base.parent == Path.home() / "Projects" and base.parent.resolve() == base.parent
    assert re.fullmatch(r"shards-managed-events-official-current-v1(?:-v[0-9]+)?", base.name)
    out = base / phase
    out.mkdir(exist_ok=False)
    budget = json.loads((base / "budget.json").read_text())
    deadline = budget["deadline_monotonic"]
    source = base / "source"
    before = {str(p.relative_to(source)): identity(p) for p in source.rglob("*") if p.is_file()}
    env = {k: v for k, v in os.environ.items() if k in ("PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL")}
    env.update(GOCACHE=str(base / "gocache"), GOMODCACHE=str(base / "gomodcache"), GOPATH=str(base / "gopath"),
               GOTMPDIR=str(base / "tmp"), TMPDIR=str(base / "tmp"), GOENV="off", GOWORK="off", GOAUTH="off",
               GOTOOLCHAIN="local", GOPROXY="https://proxy.golang.org", GOSUMDB="sum.golang.org", GOVCS="*:off",
               GOPRIVATE="", GONOPROXY="", GONOSUMDB="", GOFLAGS="-mod=readonly", GOMAXPROCS="2", GOMEMLIMIT="2GiB",
               XDG_CONFIG_HOME=str(base / "xdg-config"), XDG_CACHE_HOME=str(base / "xdg-cache"), PYTHONDONTWRITEBYTECODE="1")
    record = {"phase": phase, "base": str(base), "environment": list(platform.uname()), "budget": budget,
              "commands": [], "source_before": before, "runtime_attempts": 0, "cases": [], "retry": False,
              "fd_before": sorted(os.listdir("/proc/self/fd")), "ROOT_source": SOURCE,
              "build_environment": {k: v for k, v in env.items() if k not in ("HOME", "USER", "LOGNAME", "PATH")}}
    code = 1

    def run(name, argv, cwd=base):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError("absolute original budget expired; no next actor")
        command = {"name": name, "argv": argv, "cwd": str(cwd), "timeout_s": remaining,
                   "fd_before": sorted(os.listdir("/proc/self/fd"))}
        record["commands"].append(command)
        pidfd = None
        proc = None
        try:
            with (out / (name + ".stdout")).open("wb") as stdout, (out / (name + ".stderr")).open("wb") as stderr:
                proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
                pidfd = os.pidfd_open(proc.pid, 0)
                try:
                    status = proc.wait(timeout=max(0, deadline - time.monotonic()))
                except subprocess.TimeoutExpired:
                    proc.kill()
                    status = proc.wait(timeout=max(0, min(15, deadline - time.monotonic())))
                    command["timeout"] = True
                command.update(exit_code=status, pidfd_exit_ready=bool(select.select([pidfd], [], [], 0)[0]), direct_child_reaped=proc.returncode is not None)
        finally:
            if pidfd is not None:
                os.close(pidfd)
            command["fd_after"] = sorted(os.listdir("/proc/self/fd"))
            (out / (name + ".command.json")).write_text(json.dumps(command, indent=2) + "\n")
        command["source_after"] = {str(p.relative_to(source)): identity(p) for p in source.rglob("*") if p.is_file()}
        # Cleanup facts are retained before any outcome assertion.
        assert command["fd_before"] == command["fd_after"] and command["pidfd_exit_ready"] and command["direct_child_reaped"]
        assert command["source_after"] == before, "source changed after " + name
        if command.get("timeout") or command["exit_code"]:
            raise RuntimeError(f"{name} failed; batch terminal, no retry")
        print(json.dumps({"command": name, "exit_code": command["exit_code"]}), flush=True)

    try:
        if phase == "prepare":
            completed = read_root("completed.json")
            (out / "ROOT-completed.json").write_bytes(completed)
            assert json.loads(completed)["source"] == SOURCE
            assert json.loads(completed)["ordinary_suite"] == "26passed/0failed/10ignored"
            copied = {}
            for name in INPUTS:
                data = read_root(name)
                path = base / "inputs" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                with path.open("xb") as stream:
                    stream.write(data)
                copied[name] = {"ROOT_path": str(ROOT / name), "private_path": str(path), **identity(path)}
            record["inputs"] = copied
            for name in ("tools", "gocache", "gomodcache", "gopath", "tmp", "xdg-config", "xdg-cache"):
                (base / name).mkdir(exist_ok=False)
            go = shutil.which("go", path=env.get("PATH"))
            if not go:
                raise RuntimeError("MissingPrerequisite: installed Go>=1.26; no installation")
            record["go_binary"] = {"path": str(Path(go).resolve()), **identity(Path(go).resolve())}
            run("go-version", [go, "version"])
            match = re.search(r"go(\d+)\.(\d+)(?:\.(\d+))?", (out / "go-version.stdout").read_text())
            if not match or tuple(map(int, match.groups()[:2])) < (1, 26):
                raise RuntimeError("MissingPrerequisite: installed Go>=1.26; GOTOOLCHAIN=local/no installation")
            run("go-environment", [go, "env", "-json", "GOVERSION", "GOROOT", "GOCACHE", "GOMODCACHE", "GOAUTH", "GOENV", "GOTOOLCHAIN", "GOPROXY", "GOSUMDB"])
            oracle = source / "oracle"
            run("module-download", [go, "mod", "download", "-json"], cwd=oracle)
            run("module-verify", [go, "mod", "verify"], cwd=oracle)
            run("module-list", [go, "list", "-m", "-json", "go.opentelemetry.io/collector/pdata"], cwd=oracle)
            goroot = Path(json.loads((out / "go-environment.stdout").read_text())["GOROOT"])
            gofmt = goroot / "bin/gofmt"
            run("source-format", [str(gofmt), "-l", str(oracle / "main.go"), str(oracle / "main_test.go")])
            assert not (out / "source-format.stdout").read_bytes()
            run("source-vet", [go, "vet", "-p", "2", "./..."], cwd=oracle)
            run("oracle-build", [go, "build", "-p", "2", "-o", str(base / "tools/oracle"), "."], cwd=oracle)
            run("oracle-test-build", [go, "test", "-p", "2", "-c", "-o", str(base / "tools/oracle-tests"), "."], cwd=oracle)
            run("oracle-list", [str(base / "tools/oracle-tests"), "-test.list", "."])
            listed = (out / "oracle-list.stdout").read_text().splitlines()
            expected = re.findall(r"^func (Test\w+)\(", (oracle / "main_test.go").read_text(), re.M)
            assert listed == expected and listed.count("TestExactNumbersAndScope") == 1
            record["listed_tests"] = listed
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            checksum_url = URL + ".sha256"
            with opener.open(checksum_url, timeout=min(30, max(1, deadline - time.monotonic()))) as response:
                checksum = response.read()
            assert hashlib.sha256(checksum).hexdigest() == "b26b4931eb134e95d9f9182de894e63b0662c740572996f42d5d29d0d6ce590a"
            assert checksum.decode().split()[0] == ASSET
            (base / "tools/otelcol.tar.gz.sha256").write_bytes(checksum)
            with opener.open(URL, timeout=min(30, max(1, deadline - time.monotonic()))) as response:
                asset = response.read()
            assert hashlib.sha256(asset).hexdigest() == ASSET
            (base / "tools/otelcol.tar.gz").write_bytes(asset)
            with tarfile.open(fileobj=io.BytesIO(asset), mode="r:gz") as archive:
                candidates = [m for m in archive.getmembers() if m.isfile() and m.name == "otelcol"]
                assert len(candidates) == 1
                binary = archive.extractfile(candidates[0]).read()
            assert hashlib.sha256(binary).hexdigest() == COLLECTOR
            (base / "tools/otelcol").write_bytes(binary)
            (base / "tools/otelcol").chmod(0o700)
            record["collector_download"] = {"url": URL, "checksum_url": checksum_url, "archive": identity(base / "tools/otelcol.tar.gz"), "checksum": identity(base / "tools/otelcol.tar.gz.sha256")}
            record["artifacts"] = {name: {"path": str(base / "tools" / name), **identity(base / "tools" / name)} for name in ("oracle", "oracle-tests", "otelcol")}
            run("collector-build-info", [go, "version", "-m", str(base / "tools/otelcol")])
        else:
            prep = json.loads((base / "prepare/result.json").read_text())
            assert prep["outcome"] == 0 and prep["runtime_attempts"] == 0 and before == prep["source_after"]
            for name, artifact in prep["artifacts"].items():
                assert identity(Path(artifact["path"])) == {k: artifact[k] for k in ("bytes", "sha256")}
            for name, expected in prep["inputs"].items():
                assert identity(base / "inputs" / name) == {k: expected[k] for k in ("bytes", "sha256")}
            record["runtime_attempts"] += 1
            run("positive-oracle-unit", [str(base / "tools/oracle-tests"), "-test.run", "^TestExactNumbersAndScope$", "-test.v", "-test.count=1"])
            unit = (out / "positive-oracle-unit.stdout").read_text()
            assert re.findall(r"^=== RUN\s+(\S+)", unit, re.M) == ["TestExactNumbersAndScope"]
            assert "--- PASS: TestExactNumbersAndScope" in unit and "\nPASS\n" in unit and "SKIP" not in unit
            for index, name in enumerate(INPUTS):
                label = name.removesuffix(".json").replace("/", "-")
                path = base / "inputs" / name
                assert identity(path) == {k: prep["inputs"][name][k] for k in ("bytes", "sha256")}
                record["runtime_attempts"] += 1
                run(label + "-decode", [str(base / "tools/oracle"), "-max-bytes", str(path.stat().st_size), "-input", str(path)])
                counts = check_receipt(json.loads(path.read_bytes()), json.loads((out / (label + "-decode.stdout")).read_bytes()))
                # This is the sum of the unchanged fixture's startup/request and
                # three possible cleanup waits, not a new picked time allowance.
                if deadline - time.monotonic() < 10 + 10 + 3 * 15:
                    raise RuntimeError("insufficient original budget for unchanged Collector fixture; no next actor")
                case = out / label
                case.mkdir(exist_ok=False)
                record["runtime_attempts"] += 1
                run(label + "-collector", [sys.executable, str(base / "observe-collector.py"), str(source / "collector-http.py"), str(base / "tools/otelcol"), str(case / "pidfd.json"),
                    "--collector", str(base / "tools/otelcol"), "--input", str(path), "--out", str(case / "http"), "--collector-sha256", COLLECTOR,
                    "--startup-seconds", "10", "--request-seconds", "10", "--cleanup-seconds", "15"])
                observed = json.loads((case / "pidfd.json").read_text())
                http = json.loads((case / "http/result.json").read_text())
                assert observed["fd_before"] == observed["fd_after"] and len(observed["children"]) == 1
                child = observed["children"][0]
                assert child["pidfd_closed_before_harness_fd_check"] and child["waits"][-1] == {"returncode": 0, "pidfd_exit_ready": True, "direct_child_reaped": True}
                assert http["passed"] and http["status"] == 200 and http["collector_exit"] == 0 and http["input_bytes"] == path.stat().st_size
                assert (http["expected_resources"], http["expected_metrics"], http["expected_points"]) == (counts["resources"], counts["metrics"], counts["points"])
                logs = [json.loads(line) for line in (case / "http/collector.log").read_text().splitlines()]
                summaries = [line for line in logs if line.get("msg") == "Metrics"]
                assert len(summaries) == 1 and (summaries[0]["resource metrics"], summaries[0]["metrics"], summaries[0]["data points"]) == (counts["resources"], counts["metrics"], counts["points"])
                record["cases"].append({"input": name, "input_identity": identity(path), **counts, "http": 200, "collector_counts_equal": True, "child_reaped": True})
                print(json.dumps({"case": name, "passed": True}), flush=True)
            record["artifacts_after"] = {name: identity(Path(artifact["path"])) for name, artifact in prep["artifacts"].items()}
            assert len(record["cases"]) == len(INPUTS)
        assert {str(p.relative_to(source)): identity(p) for p in source.rglob("*") if p.is_file()} == before
        code = 0
    except Exception as error:
        record["failure"] = {"type": type(error).__name__, "message": str(error)}
    finally:
        record["outcome"] = code
        record["source_after"] = {str(p.relative_to(source)): identity(p) for p in source.rglob("*") if p.is_file()}
        record["fd_after"] = sorted(os.listdir("/proc/self/fd"))
        record["fd_sets_include_enumeration"] = True
        (out / "result.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
        # The nine exact input bytes belong in the preparation evidence too.
        if phase == "prepare" and (base / "inputs").exists():
            for path in (base / "inputs").rglob("*.json"):
                target = out / "copied-inputs" / path.relative_to(base / "inputs")
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(path.read_bytes())
        (out / "members.json").write_text(json.dumps({str(p.relative_to(out)): identity(p) for p in out.rglob("*") if p.is_file()}, indent=2) + "\n")
        archive_path = base / (phase + "-raw.tar.gz")
        with tarfile.open(archive_path, "w:gz") as archive:
            for path in sorted(out.rglob("*")):
                if path.is_file():
                    archive.add(path, arcname=str(path.relative_to(out)), recursive=False)
        print(json.dumps({"phase": phase, "outcome": code, "failure": record.get("failure"), "cases": len(record["cases"]),
                          "runtime_attempts": record["runtime_attempts"], "archive": identity(archive_path), "base": str(base)}), flush=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
